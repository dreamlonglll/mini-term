//! 语法高亮扩展包(扩展 `syntax-pack`)的宿主侧:找库 → 核哈希 → 加载 → 主线程注册。
//!
//! 语法包是 `crates/mt-syntax-pack` 编出的动态库,表结构与语言清单见 `mt-syntax-pack-abi`。
//! 它带着 C++ / PHP / Ruby / C# / Kotlin … 共 16 种语言(体积大头,约 31 MB),
//! 不随安装包发,按需下载;**不在时这些语言退成纯文本,不报错**(`language_for` 照常返回
//! 名字,注册表查不到就是 Plain)。
//!
//! # 流程([`init`],启动时调一次)
//!
//! 1. 后台线程:`mt_extensions::global().locate(SyntaxPack, 平台库文件名)` —— exe 同目录的
//!    旧布局(dev / 源码构建,与 exe 同信任等级,不核哈希)或本机已装扩展(先按清单的
//!    sha256 + 字节数**整文件核一遍**,核过才加载)。
//! 2. 同一后台线程:加载库(Windows 用 `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
//!    LOAD_LIBRARY_SEARCH_SYSTEM32`,不走 PATH / 当前目录)→ 调入口拿表 → [`parse_table`]
//!    校验(ABI 版本、表项尺寸、字符串 UTF-8、名字白名单、`Language::abi_version()` 在本进程
//!    tree-sitter 支持区间内)并建好 `LanguageConfig`(`ZED_REWRITE` 的查询在这里改写)。
//!    库句柄**永不卸载**(`mem::forget`):语言对象、外部扫描器、查询原文全住在库里。
//! 3. 主线程:注册进 `LanguageRegistry`(本名 + [`PackLanguage::aliases`] 逐个注册成键 ——
//!    组件库自己的别名解析只认编进组件库的语言),再让「语法包到位前就打开了的」编辑器
//!    重新套一次语言([`watch_editor`])。Markdown 代码围栏不用管:组件库按注册表的
//!    generation 自己失效重算。
//!
//! 同一进程只加载一次。在设置 →「扩展」页下载 / 导入语法包,或在文件预览的顶部提示条
//! (`file_viewer::render_syntax_pack_banner`,走 [`crate::extensions::Origin::SyntaxBanner`])
//! 点下载后,扩展服务的事件泵调 [`on_extensions_changed`] 当场加载,不必重启。
use std::cell::RefCell;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{App, Entity, WeakEntity};
use gpui_component::highlighter::{LanguageConfig, LanguageRegistry};
use gpui_component::input::EditorState;
use mt_extensions::ComponentId;
use mt_syntax_pack_abi::{self as abi, AbiStr, AbiStrList, PackLanguage, PackTable};
use tree_sitter_language::LanguageFn;

use crate::syntax_languages::{demote_catch_alls, zed_captures};

/// 单个字符串的上限(查询原文最大的也就几十 KB;防坏表让我们去读半个进程的内存)。
const MAX_STR_BYTES: usize = 4 * 1024 * 1024;
/// 表项数与注入目标数的上限(同上,防坏表)。
const MAX_ITEMS: usize = 256;

/// 语法包在本进程里的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackState {
    /// 还没找完(启动早期)。
    Pending,
    /// 本机没有语法包:这些语言是纯文本。
    Missing,
    /// 已注册。
    Loaded,
    /// 找到了但没加载成(哈希不符 / 版本不对 / 库坏了),原因进日志。语言同样是纯文本。
    Failed(String),
}

static STATE: Mutex<PackState> = Mutex::new(PackState::Pending);

fn set_state(state: PackState) {
    *STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
}

/// 当前状态(提示条判「要不要提示下载」用)。
pub fn state() -> PackState {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// `lang`(`language_for` 的返回值)是语法包提供的语言、而本机确实没有语法包(`Missing`)
/// → 它的清单条目(显示名等)。文件预览的下载提示条用;`Pending`(启动还没找完)与
/// `Failed`(装了但坏了,下载也没用)不提示。
pub fn missing_language(lang: &str) -> Option<&'static PackLanguage> {
    let language = abi::find_language(lang)?;
    (state() == PackState::Missing).then_some(language)
}

/// 提示条「关闭」只管本次会话(进程级)。
static BANNER_DISMISSED: AtomicBool = AtomicBool::new(false);
/// 提示条点过「下载」、结果还没回来。
static DOWNLOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn banner_dismissed() -> bool {
    BANNER_DISMISSED.load(Ordering::Relaxed)
}

pub fn dismiss_banner() {
    BANNER_DISMISSED.store(true, Ordering::Relaxed);
}

pub fn download_requested() -> bool {
    DOWNLOAD_REQUESTED.load(Ordering::Relaxed)
}

/// 清单里语法包扩展的下载体积(人读);没清单 / 本平台没有语法包 = `None`(提示条不出:
/// dev / 源码构建没有在线下载)。
pub fn download_size() -> Option<String> {
    let component = mt_extensions::global().component(ComponentId::SyntaxPack)?;
    Some(crate::extensions::format_size(component.size))
}

/// 提示条的「下载」:走扩展服务(去重、校验、安装),装好后由 [`on_extensions_changed`]
/// 当场加载。进度与失败原因走扩展服务那条原地更新的 toast(`Origin::SyntaxBanner`:用户的
/// 显式动作,无冷却)。
pub fn request_download(cx: &mut App) {
    if DOWNLOAD_REQUESTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let receiver = crate::extensions::ensure(
        ComponentId::SyntaxPack,
        crate::extensions::Origin::SyntaxBanner,
    );
    // 接收端是 oneshot:直接在 GPUI 任务里 await,不占线程(下载有自己的线程)。
    cx.spawn(async move |cx| {
        let result = receiver.await;
        cx.update(|cx| {
            DOWNLOAD_REQUESTED.store(false, Ordering::Relaxed);
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!("[syntax-pack] download did not finish: {result:?}");
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// 一种解析好、待注册的语言。`LanguageConfig` 是 Send 的,后台建好交给主线程。
struct Parsed {
    language: &'static PackLanguage,
    config: LanguageConfig,
}

/// 启动时调一次:后台找库、核哈希、加载、解析,回主线程注册。
///
/// 在跑期间状态是 `Pending`,[`on_extensions_changed`] 不会再起第二趟(两趟可能加载两个
/// 不同路径的库,而句柄永不卸载)。
pub fn init(cx: &mut App) {
    set_state(PackState::Pending);
    let task = cx.background_executor().spawn(async { locate_and_load() });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| finish(result, cx));
    })
    .detach();
}

/// 扩展服务的状态缓存变了(下载 / 导入 / 删除之后,`extensions::dispatch` 调):
/// 语法包从「没有」变成「本机可用」就当场加载,不必重启。只读状态缓存,不做 IO。
///
/// `Failed`(哈希不符 / 版本不对)不自动重试 —— 每批事件都重核一遍 31 MB 不值;用户在
/// 设置页删掉(缓存变回未安装)后状态回到 `Missing`,重新下载即重试。
pub fn on_extensions_changed(cx: &mut App) {
    let available = crate::extensions::cached_available(ComponentId::SyntaxPack);
    match state() {
        PackState::Missing if available => init(cx),
        PackState::Failed(_) if !available => set_state(PackState::Missing),
        _ => {}
    }
}

fn finish(result: Result<Option<Vec<Parsed>>, String>, cx: &mut App) {
    match result {
        Ok(Some(parsed)) => {
            register(&parsed);
            set_state(PackState::Loaded);
            tracing::info!("[syntax-pack] registered {} languages", parsed.len());
            refresh_watched(cx);
            // Markdown 预览的代码围栏按注册表 generation 失效,但要等下一次重画才生效
            cx.refresh_windows();
        }
        Ok(None) => set_state(PackState::Missing),
        Err(error) => {
            tracing::warn!("[syntax-pack] not loaded: {error}");
            set_state(PackState::Failed(error));
        }
    }
}

fn register(parsed: &[Parsed]) {
    let registry = LanguageRegistry::singleton();
    for item in parsed {
        registry.register(item.language.name, &item.config);
        for alias in item.language.aliases {
            registry.register(alias, &item.config);
        }
    }
}

fn locate_and_load() -> Result<Option<Vec<Parsed>>, String> {
    let file = abi::library_file_name();
    let located = mt_extensions::global()
        .locate(ComponentId::SyntaxPack, file, None)
        .map_err(|error| format!("locate {file}: {error}"))?;
    let Some(located) = located else {
        return Ok(None);
    };
    if let Some((sha256, size)) = &located.expected {
        verify_file(&located.path, sha256, *size)?;
    }
    load_library(&located.path).map(Some)
}

/// 整文件 sha256 + 字节数与清单对账(扩展目录里的文件可能被改过)。
fn verify_file(path: &Path, sha256: &str, size: u64) -> Result<(), String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > size {
            return Err(format!(
                "{}: larger than manifest size {size}",
                path.display()
            ));
        }
        context.update(&buffer[..read]);
    }
    if total != size {
        return Err(format!(
            "{}: size {total} != manifest {size}",
            path.display()
        ));
    }
    let digest = context.finish();
    let actual: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(sha256) {
        return Err(format!("{}: sha256 mismatch", path.display()));
    }
    Ok(())
}

fn load_library(path: &Path) -> Result<Vec<Parsed>, String> {
    // SAFETY: 加载会执行库的初始化代码。库来自 exe 同目录(与 exe 同信任等级)或扩展目录
    // (上一步已按清单核过哈希),与主程序同一次 CI 构建产出。
    let library = unsafe { open_library(path) }
        .map_err(|error| format!("load {}: {error}", path.display()))?;
    // SAFETY: 入口的签名由 ABI crate 定义,语法包用同一个类型导出。
    let entry: abi::EntryFn = *unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL_NUL) }
        .map_err(|error| format!("{}: {error}", abi::ENTRY_SYMBOL))?;
    // SAFETY: 同上;入口只返回静态表的地址。
    let table = unsafe { entry() };
    // SAFETY: 表在库句柄存活期间有效,而句柄下面就 forget 掉、永不卸载。
    let parsed = unsafe { parse_table(table) }?;
    // 永不卸载:语言对象(TSLanguage 静态数据)、外部扫描器函数、查询原文都在库里,
    // 注册表与已建的解析器会一直引用它们。
    std::mem::forget(library);
    Ok(parsed)
}

/// # Safety
///
/// 加载动态库会执行它的初始化代码,调用方保证库可信。
#[cfg(windows)]
unsafe fn open_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, Library,
    };
    // 依赖只从库自己的目录与 System32 找:不走 PATH / 当前目录(DLL 劫持面)。
    // `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` 要求绝对路径,`locate` 给的都是。
    unsafe {
        Library::load_with_flags(
            path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map(Into::into)
}

/// # Safety
///
/// 同上。
#[cfg(unix)]
unsafe fn open_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
    // RTLD_LOCAL:库里的符号不进全局命名空间(与宿主的 tree-sitter 符号互不干扰)。
    unsafe { Library::open(Some(path), RTLD_NOW | RTLD_LOCAL) }.map(Into::into)
}

/// 读一段表里的字符串并校验 UTF-8。
///
/// # Safety
///
/// `s` 来自表,`len > 0` 时指向至少 `len` 字节、生命周期 = 库句柄(永不卸载)。
unsafe fn read_str(s: AbiStr, what: &str) -> Result<&'static str, String> {
    if s.len == 0 {
        return Ok("");
    }
    if s.ptr.is_null() || s.len > MAX_STR_BYTES {
        return Err(format!("{what}: bad string (len {})", s.len));
    }
    // SAFETY: 见函数说明。
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    std::str::from_utf8(bytes).map_err(|error| format!("{what}: not UTF-8 ({error})"))
}

/// # Safety
///
/// 同 [`read_str`]。
unsafe fn read_list(list: AbiStrList, what: &str) -> Result<Vec<&'static str>, String> {
    if list.len == 0 {
        return Ok(Vec::new());
    }
    if list.ptr.is_null() || list.len > MAX_ITEMS {
        return Err(format!("{what}: bad list (len {})", list.len));
    }
    // SAFETY: 见函数说明。
    let items = unsafe { std::slice::from_raw_parts(list.ptr, list.len) };
    items
        .iter()
        // SAFETY: 同上。
        .map(|item| unsafe { read_str(*item, what) })
        .collect()
}

/// 校验整张表并建好待注册的配置。名字不在 [`abi::LANGUAGES`] 白名单里的条目、
/// tree-sitter ABI 不在本进程支持区间的语言**跳过**(进日志),不让整包作废;表头不对
/// (版本 / 表项尺寸 / 空指针)整包拒绝。
///
/// # Safety
///
/// `table` 来自语法包入口(或单测里的同构静态表),表及其指向的数据在调用期间
/// 与之后(库永不卸载)都有效。
unsafe fn parse_table(table: *const PackTable) -> Result<Vec<Parsed>, String> {
    // SAFETY: 见函数说明。
    let Some(table) = (unsafe { table.as_ref() }) else {
        return Err("entry returned null".to_string());
    };
    if table.abi_version != abi::ABI_VERSION {
        return Err(format!(
            "ABI version {} (host expects {})",
            table.abi_version,
            abi::ABI_VERSION
        ));
    }
    if table.entry_size as usize != size_of::<abi::LanguageEntry>() {
        return Err(format!("entry size {} mismatch", table.entry_size));
    }
    if table.languages_len > MAX_ITEMS || (table.languages.is_null() && table.languages_len > 0) {
        return Err(format!("bad language list (len {})", table.languages_len));
    }
    if table.languages_len == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: 见函数说明,长度已限幅。
    let entries = unsafe { std::slice::from_raw_parts(table.languages, table.languages_len) };
    let mut parsed: Vec<Parsed> = Vec::with_capacity(entries.len());
    for entry in entries {
        // SAFETY: 字符串都来自同一张表。
        let name = unsafe { read_str(entry.name, "name") }?;
        let Some(language) = abi::LANGUAGES.iter().find(|lang| lang.name == name) else {
            tracing::warn!("[syntax-pack] skip unknown language {name:?}");
            continue;
        };
        if parsed.iter().any(|item| item.language.name == name) {
            continue;
        }
        // SAFETY: 函数指针来自语法 crate 的 `LANGUAGE` 常量(`LanguageFn::into_raw`)。
        let grammar: tree_sitter::Language = unsafe { LanguageFn::from_raw(entry.language) }.into();
        let version = grammar.abi_version();
        if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION)
            .contains(&version)
        {
            tracing::warn!("[syntax-pack] skip {name}: tree-sitter ABI {version}");
            continue;
        }
        // SAFETY: 同上。
        let (highlights, injections, locals, injection_languages) = unsafe {
            (
                read_str(entry.highlights, name)?,
                read_str(entry.injections, name)?,
                read_str(entry.locals, name)?,
                read_list(entry.injection_languages, name)?,
            )
        };
        let highlights = if entry.flags & abi::flags::ZED_REWRITE != 0 {
            demote_catch_alls(&zed_captures(highlights))
        } else {
            highlights.to_string()
        };
        let config = LanguageConfig::new(
            language.name,
            grammar,
            injection_languages.into_iter().map(Into::into).collect(),
            &highlights,
            injections,
            locals,
        );
        parsed.push(Parsed { language, config });
    }
    Ok(parsed)
}

thread_local! {
    /// 语法包到位前用语法包语言建出来的编辑器:到位后给它们重新套一次语言。
    static WATCHED: RefCell<Vec<(&'static str, WeakEntity<EditorState>)>> =
        const { RefCell::new(Vec::new()) };
}

/// 文件预览建好(或改名换了语言的)编辑器后调:`lang` 是语法包语言而语法包还没到位时
/// 记下它,到位后 [`refresh_watched`] 重新 `set_highlighter`。其余情况什么都不做。
pub fn watch_editor(lang: &'static str, editor: &Entity<EditorState>) {
    if abi::find_language(lang).is_none() || state() == PackState::Loaded {
        return;
    }
    WATCHED.with(|watched| {
        let mut watched = watched.borrow_mut();
        watched.retain(|(_, editor)| editor.upgrade().is_some());
        watched.push((lang, editor.downgrade()));
    });
}

fn refresh_watched(cx: &mut App) {
    let watched = WATCHED.with(|watched| std::mem::take(&mut *watched.borrow_mut()));
    for (lang, editor) in watched {
        if let Some(editor) = editor.upgrade() {
            editor.update(cx, |state, cx| state.set_highlighter(lang, cx));
        }
    }
}

/// 单测用:不经动态库,把语法包 rlib 里同一张表解析 + 注册进注册表单例。幂等。
#[cfg(test)]
pub(crate) fn register_bundled_for_tests() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::syntax_languages::register();
        // SAFETY: rlib 里的静态表,进程内永远有效。
        let parsed = unsafe { parse_table(mt_syntax_pack::table()) }.expect("语法包表解析");
        assert_eq!(parsed.len(), abi::LANGUAGES.len(), "表里每种语言都该收下");
        register(&parsed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax_languages::tests::assert_colored_as;

    /// 宿主改写(`ZED_REWRITE`)之后的查询再编一遍:语法包单测只编得到原文。
    #[test]
    fn 注册后每种语言与别名都查得到且查询能编译() {
        register_bundled_for_tests();
        let registry = LanguageRegistry::singleton();
        let mut failed = Vec::new();
        for lang in abi::LANGUAGES {
            for name in std::iter::once(&lang.name).chain(lang.aliases) {
                let config = registry
                    .language(name)
                    .unwrap_or_else(|| panic!("注册表查不到 {name}"));
                assert_eq!(config.name.as_ref(), lang.name, "{name} 指向本名");
                assert!(!config.highlights.is_empty(), "{name}: 高亮查询为空");
            }
            let config = registry.language(lang.name).expect(lang.name);
            let grammar = config.language.as_ref().expect("带解析器");
            let source = format!(
                "{}\n{}\n{}",
                config.injections, config.locals, config.highlights
            );
            if let Err(error) = tree_sitter::Query::new(grammar, &source) {
                failed.push(format!("{}: {error}", lang.name));
            }
        }
        assert!(
            failed.is_empty(),
            "改写后编不过的查询:\n{}",
            failed.join("\n")
        );
    }

    #[test]
    fn 常用别名在语法包语言上照样查得到() {
        register_bundled_for_tests();
        let registry = LanguageRegistry::singleton();
        for (alias, name) in [
            ("cs", "csharp"),
            ("c++", "cpp"),
            ("rb", "ruby"),
            ("kt", "kotlin"),
            ("kts", "kotlin"),
            ("ex", "elixir"),
            ("phtml", "php"),
            ("protobuf", "proto"),
        ] {
            let config = registry
                .language(alias)
                .unwrap_or_else(|| panic!("{alias}"));
            assert_eq!(config.name.as_ref(), name);
        }
    }

    #[test]
    fn 表头不对整包拒绝() {
        let good = *mt_syntax_pack::table();
        let wrong_version = PackTable {
            abi_version: abi::ABI_VERSION + 1,
            ..good
        };
        // SAFETY: 指向 rlib 里的静态表项。
        assert!(unsafe { parse_table(&wrong_version) }.is_err());
        let wrong_size = PackTable {
            entry_size: good.entry_size + 8,
            ..good
        };
        // SAFETY: 同上。
        assert!(unsafe { parse_table(&wrong_size) }.is_err());
        // SAFETY: 空指针由函数自己判。
        assert!(unsafe { parse_table(std::ptr::null()) }.is_err());
    }

    #[test]
    fn 白名单外的名字跳过_不许覆盖内置语言() {
        let good = mt_syntax_pack::table();
        // SAFETY: rlib 里的静态表项。
        let mut entries =
            unsafe { std::slice::from_raw_parts(good.languages, good.languages_len) }.to_vec();
        entries[0].name = AbiStr::new("rust");
        let table = PackTable {
            languages: entries.as_ptr(),
            languages_len: entries.len(),
            ..*good
        };
        // SAFETY: `entries` 在本函数内存活。
        let parsed = unsafe { parse_table(&table) }.expect("表头没问题");
        assert_eq!(parsed.len(), abi::LANGUAGES.len() - 1);
        assert!(parsed.iter().all(|item| item.language.name != "rust"));
    }

    /// 真走一遍动态库加载:dev-dependency 的 `mt-syntax-pack` 编 rlib 时 cdylib 一并产出在
    /// `target/<profile>/deps/`,与本单测 exe 同目录。加载 → 解析表 → 用宿主的 tree-sitter
    /// 运行时解析一段 Ruby(带外部扫描器,验跨模块调用)。不改注册表单例。
    #[test]
    fn 真实动态库加载解析() {
        let exe = std::env::current_exe().expect("current_exe");
        let path = exe.with_file_name(abi::library_file_name());
        if !path.exists() {
            eprintln!("跳过:{} 不在(cdylib 没随 rlib 产出?)", path.display());
            return;
        }
        let parsed = load_library(&path).expect("加载语法包动态库");
        assert_eq!(parsed.len(), abi::LANGUAGES.len());
        let ruby = parsed
            .iter()
            .find(|item| item.language.name == "ruby")
            .expect("ruby");
        let grammar = ruby.config.language.clone().expect("带解析器");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar)
            .expect("宿主运行时认得这份语法");
        let tree = parser
            .parse("x = <<~EOS\n  heredoc\nEOS\nputs x\n", None)
            .expect("解析");
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        tree_sitter::Query::new(&grammar, &ruby.config.highlights).expect("查询能编");
    }

    #[test]
    fn 坏字符串报错() {
        let bad = [0xffu8, 0xfe];
        let s = AbiStr {
            ptr: bad.as_ptr(),
            len: bad.len(),
        };
        // SAFETY: 指向本地数组。
        assert!(unsafe { read_str(s, "x") }.is_err());
        let null = AbiStr {
            ptr: std::ptr::null(),
            len: 3,
        };
        // SAFETY: 空指针由函数自己判。
        assert!(unsafe { read_str(null, "x") }.is_err());
    }

    #[test]
    fn 哈希与字节数对账() {
        let dir =
            std::env::temp_dir().join(format!("mt-syntax-pack-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.bin");
        std::fs::write(&path, b"abc").unwrap();
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_file(&path, sha, 3).is_ok());
        assert!(verify_file(&path, &sha.to_uppercase(), 3).is_ok());
        assert!(verify_file(&path, sha, 4).is_err(), "字节数不符");
        assert!(
            verify_file(&path, &sha.replace('b', "c"), 3).is_err(),
            "哈希不符"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C# 是当初补语言的由头:方法名 function 色、关键字 keyword 色、字符串 string 色。
    /// 顺带验「同一节点先到的名字赢」这条组件库规则下 Zed 的查询(兜底
    /// `(identifier) @variable` 在最前)仍然对。(自 syntax_languages 挪来)
    #[test]
    fn csharp_端到端上色() {
        register_bundled_for_tests();
        let code =
            "using System;\nclass Program { static void Main() { Console.WriteLine(\"hi\"); } }";
        assert_colored_as("csharp", code, "Main", "function");
        assert_colored_as("csharp", code, "WriteLine", "function");
        assert_colored_as("csharp", code, "class", "keyword");
        assert_colored_as("csharp", code, "\"hi\"", "string");
        assert_colored_as("csharp", code, "Program", "type");
    }

    /// crate 自带的 nvim 风格查询经捕获名翻译后能上色:`this` 是 variable.special。
    /// (自 syntax_languages 挪来)
    #[test]
    fn kotlin_捕获名翻译后上色() {
        register_bundled_for_tests();
        let code = "class A { fun f(xs: List<Int>) = this.g(xs, \"s\") }";
        assert_colored_as("kotlin", code, "this", "variable.special");
        assert_colored_as("kotlin", code, "fun", "keyword");
        assert_colored_as("kotlin", code, "List", "type");
        assert_colored_as("kotlin", code, "\"s\"", "string");
    }

    /// CMake 补上查询后命令名 / 字符串有颜色。(自 syntax_languages 挪来)
    #[test]
    fn cmake_补上查询后上色() {
        register_bundled_for_tests();
        let code = "set(MY_FLAG ON)\nmessage(STATUS \"x\")";
        assert_colored_as("cmake", code, "set", "function");
        assert_colored_as("cmake", code, "\"x\"", "string");
    }

    /// 组件库来源的语言(不改写)也能上色:Ruby 的 `def` 是关键字。
    #[test]
    fn ruby_组件库口径查询上色() {
        register_bundled_for_tests();
        let code = "def hello\n  puts \"hi\"\nend";
        assert_colored_as("ruby", code, "def", "keyword");
        assert_colored_as("ruby", code, "\"hi\"", "string");
    }
}

//! # mt-i18n —— GPUI 侧的双语文案层
//!
//! 把 mini-term 自研 zustand i18n（`src/i18n/`）整套搬到 Rust：
//! 同一份命名空间 / key 层级、同一套 `{name}` 插值语法、同一条「缺 key 回落中文」规则，
//! 所以 Wave 3.5 替换硬编码文案时，TS 侧的 `t('app.menu.settings')` 一对一变成
//! `t("app", "menu.settings")`，不需要重新翻译、不需要改 key。
//!
//! ## 三行上手
//!
//! ```
//! use mt_i18n::{t, tr, Locale, set_locale};
//!
//! set_locale(Locale::Zh);
//! assert_eq!(t("app", "menu.settings"), "设置");
//! assert_eq!(tr!("app", "update.badge", version = "0.14.0"), "新版本 0.14.0");
//! ```
//!
//! ## 设计要点
//!
//! - **字典是编译期静态数据**（`dict.rs`，由 `tools/gen_from_ts.mjs` 生成）：
//!   全部进 rodata，无运行时解析、无初始化、无堆分配，查表是二分查找。
//! - **零第三方依赖**（serde 是可选 feature）：任何 crate 都能依赖它而不拖进依赖树。
//!   特别是**不依赖 gpui** —— 后端 crate（mt-pty / mt-relay 的错误消息）同样能用。
//! - **全局当前语言用原子量**：读路径无锁，UI 每帧调几千次也不心疼。
//! - **持久化不归本 crate 管**：只暴露 [`locale`] / [`set_locale`]，
//!   写进 `AppConfig` 是 mt-config / mt-app 的事（对应 TS 侧的 localStorage）。

pub mod dict;

use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

// ---------------------------------------------------------------------------
// Locale
// ---------------------------------------------------------------------------

/// 界面语言。取值与 TS 侧 `Lang = 'zh' | 'en'` 一一对应。
///
/// 默认 [`Locale::Zh`]：与 TS 侧 `detectInitialLang()` 的最终兜底一致
/// （探测不出系统语言时用中文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Locale {
    /// 简体中文
    #[default]
    Zh,
    /// English
    En,
}

impl Locale {
    /// 全部语言，语言切换下拉框可直接遍历
    pub const ALL: [Locale; 2] = [Locale::Zh, Locale::En];

    /// 语言代码：`"zh"` / `"en"`。与 TS 侧 localStorage 里存的值相同，可直接互读。
    pub const fn code(self) -> &'static str {
        match self {
            Locale::Zh => "zh",
            Locale::En => "en",
        }
    }

    /// 该语言的自称，用于语言切换菜单（永远显示母语名，不随当前语言变）
    pub const fn native_name(self) -> &'static str {
        match self {
            Locale::Zh => "中文",
            Locale::En => "English",
        }
    }

    /// BCP 47 标签：`"zh-CN"` / `"en"`。对应 TS 侧写进 `<html lang>` 的值。
    pub const fn bcp47(self) -> &'static str {
        match self {
            Locale::Zh => "zh-CN",
            Locale::En => "en",
        }
    }

    /// 解析语言代码，只认 `"zh"` / `"en"`（大小写不敏感）
    pub fn from_code(code: &str) -> Option<Locale> {
        match code.trim().to_ascii_lowercase().as_str() {
            "zh" => Some(Locale::Zh),
            "en" => Some(Locale::En),
            _ => None,
        }
    }

    /// 从系统语言标签推断，规则同 TS 侧：`zh*` → 中文，其余一律英文。
    ///
    /// 认得 `zh`、`zh-CN`、`zh_TW.UTF-8`、`Chinese (Simplified)_China.936` 这类写法。
    pub fn from_system_tag(tag: &str) -> Locale {
        let lower = tag.trim().to_ascii_lowercase();
        if lower.starts_with("zh") || lower.starts_with("chinese") {
            Locale::Zh
        } else {
            Locale::En
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Locale::Zh => 0,
            Locale::En => 1,
        }
    }

    fn from_u8(v: u8) -> Locale {
        match v {
            1 => Locale::En,
            _ => Locale::Zh,
        }
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::str::FromStr for Locale {
    type Err = UnknownLocale;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Locale::from_code(s).ok_or(UnknownLocale)
    }
}

/// [`Locale::from_str`] 的错误类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownLocale;

impl std::fmt::Display for UnknownLocale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unknown locale code (expected \"zh\" or \"en\")")
    }
}

impl std::error::Error for UnknownLocale {}

// ---------------------------------------------------------------------------
// 全局当前语言
// ---------------------------------------------------------------------------

/// 当前语言。原子量而非 RwLock：`t()` 在 UI 每帧被调数千次，读路径必须无锁。
static CURRENT: AtomicU8 = AtomicU8::new(0); // 0 == Locale::Zh

/// 语言变化订阅者。只在切换语言时（人一辈子点不了几次）加写锁，读路径完全不碰。
static OBSERVERS: RwLock<Vec<fn(Locale)>> = RwLock::new(Vec::new());

/// 读取当前语言。线程安全，任何 crate、任何线程都能调。
#[inline]
pub fn locale() -> Locale {
    Locale::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// 切换当前语言，返回**是否真的变了**（没变时不通知订阅者）。
///
/// 本 crate 只管进程内的运行时状态；写进 `AppConfig`、重启后恢复，是上层的事。
pub fn set_locale(next: Locale) -> bool {
    let prev = CURRENT.swap(next.as_u8(), Ordering::Relaxed);
    if prev == next.as_u8() {
        return false;
    }
    // 先复制一份再回调：回调里若又调 add_locale_observer 不会死锁。
    let observers: Vec<fn(Locale)> = match OBSERVERS.read() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    for f in observers {
        f(next);
    }
    true
}

/// 按语言代码切换（`"zh"` / `"en"`），代码不认识时返回 `false` 且不改动当前语言
pub fn set_locale_from_code(code: &str) -> bool {
    match Locale::from_code(code) {
        Some(l) => {
            set_locale(l);
            true
        }
        None => false,
    }
}

/// 订阅语言变化。GPUI 侧用来在切换语言时刷新窗口（`cx.refresh()`）。
///
/// 只收 `fn` 指针不收闭包：静态全局里塞 `Box<dyn Fn>` 会引入销毁顺序问题，
/// 而实际需求（重绘、同步 gpui-component 的 rust-i18n locale）都能用函数指针表达。
pub fn add_locale_observer(f: fn(Locale)) {
    match OBSERVERS.write() {
        Ok(mut g) => g.push(f),
        Err(poisoned) => poisoned.into_inner().push(f),
    }
}

/// 从环境变量推断语言（`LC_ALL` → `LC_MESSAGES` → `LANG` → `LANGUAGE`）。
///
/// Unix 上够用；**Windows 上这些变量通常不存在**，会回落到 [`Locale::Zh`]，
/// 需要系统语言的话由 mt-app 调 Win32 `GetUserDefaultLocaleName` 后走
/// [`Locale::from_system_tag`]。
pub fn detect_from_env() -> Locale {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
            && v != "C"
            && v != "POSIX"
        {
            return Locale::from_system_tag(&v);
        }
    }
    Locale::default()
}

// ---------------------------------------------------------------------------
// 字典结构
// ---------------------------------------------------------------------------

/// 一个命名空间的双语条目表（对应 TS 侧一个 `locales/<ns>.ts`）。
///
/// 两个切片都按 key 升序排列，由生成器保证，[`Namespace::get`] 依赖此不变量做二分。
#[derive(Debug, Clone, Copy)]
pub struct Namespace {
    /// 命名空间名，如 `"app"` / `"settings"`
    pub name: &'static str,
    /// 中文条目，按 key 升序
    pub zh: &'static [(&'static str, &'static str)],
    /// 英文条目，按 key 升序
    pub en: &'static [(&'static str, &'static str)],
}

impl Namespace {
    /// 取指定语言的全部条目
    pub fn entries(&self, locale: Locale) -> &'static [(&'static str, &'static str)] {
        match locale {
            Locale::Zh => self.zh,
            Locale::En => self.en,
        }
    }

    /// 在本命名空间里查 key，不做任何回落
    pub fn get(&self, locale: Locale, key: &str) -> Option<&'static str> {
        let table = self.entries(locale);
        table
            .binary_search_by(|(k, _)| (*k).cmp(key))
            .ok()
            .map(|i| table[i].1)
    }
}

/// 全部命名空间（按 name 升序）。语言完整性测试与调试工具用。
pub fn namespaces() -> &'static [Namespace] {
    dict::NAMESPACES
}

/// 按名字取命名空间
pub fn namespace(ns: &str) -> Option<&'static Namespace> {
    dict::NAMESPACES
        .binary_search_by(|n| n.name.cmp(ns))
        .ok()
        .map(|i| &dict::NAMESPACES[i])
}

// ---------------------------------------------------------------------------
// 查表
// ---------------------------------------------------------------------------

/// 原始查表：不回落、不断言、不 panic，key 可以是运行时拼出来的。
///
/// 需要「按状态拼 key」这类动态查找时用它，例如
/// `lookup(locale(), "app", &format!("titleBar.status.{status}"))`。
pub fn lookup(locale: Locale, ns: &str, key: &str) -> Option<&'static str> {
    namespace(ns)?.get(locale, key)
}

/// 翻译。回落链与 TS 侧 `translate()` 完全一致：
/// **当前语言 → 中文 → 原样返回 key**。
///
/// 前两步任一命中都不算异常；两步全落空说明 key 写错或字典漏了条目，
/// debug 构建直接断言炸出来，release 构建退化成显示 key（界面难看但不崩）。
///
/// 参数收 `&'static str` 是刻意的：调用点几乎全是字面量，
/// 这样才能在查不到时把 key 本身作为 `&'static str` 返回。
/// 动态 key 请用 [`lookup`]。
pub fn t(ns: &'static str, key: &'static str) -> &'static str {
    t_in(locale(), ns, key)
}

/// [`t`] 的指定语言版本（不读全局状态，测试与并排预览用）
pub fn t_in(locale: Locale, ns: &'static str, key: &'static str) -> &'static str {
    if let Some(s) = lookup(locale, ns, key) {
        return s;
    }
    if locale != Locale::Zh
        && let Some(s) = lookup(Locale::Zh, ns, key)
    {
        // 英文缺条目时静默回落中文：界面上是中英混排，但不崩、不留空白。
        // 生成器已核对两语言 key 集合完全一致（见 tests/consistency.rs），
        // 真走到这里说明字典退化了，debug 下也该炸。
        debug_assert!(false, "mt-i18n: 英文字典缺条目 {ns}.{key}，已回落中文");
        return s;
    }
    debug_assert!(false, "mt-i18n: 未知文案 key {ns}.{key}");
    key
}

/// 带插值的翻译：把文案里的 `{name}` 换成 `args` 里同名的值。
///
/// ```
/// use mt_i18n::{t_args_in, Locale};
/// assert_eq!(
///     t_args_in(Locale::Zh, "time", "minutesAgo", &[("n", "5")]),
///     "5 分钟前"
/// );
/// ```
///
/// 通常用 [`tr!`] 宏更顺手（自动 `to_string` 各类值）。
pub fn t_args(ns: &'static str, key: &'static str, args: &[(&str, &str)]) -> String {
    t_args_in(locale(), ns, key, args)
}

/// [`t_args`] 的指定语言版本
pub fn t_args_in(
    locale: Locale,
    ns: &'static str,
    key: &'static str,
    args: &[(&str, &str)],
) -> String {
    interpolate(t_in(locale, ns, key), args)
}

/// 点分全路径版：`t_path("app.menu.settings")`，第一段是命名空间。
///
/// 存在的意义是让 Wave 3.5 能机械照搬 TS 侧的 `t('app.menu.settings')` 调用点，
/// 减少人肉拆分 ns 时拆错的机会；新写的代码建议直接用两段式的 [`t`]。
pub fn t_path(path: &'static str) -> &'static str {
    match path.split_once('.') {
        Some((ns, key)) => t_in(locale(), ns, key),
        None => {
            debug_assert!(false, "mt-i18n: 文案路径缺命名空间：{path}");
            path
        }
    }
}

/// [`t_path`] 的插值版
pub fn t_path_args(path: &'static str, args: &[(&str, &str)]) -> String {
    match path.split_once('.') {
        Some((ns, key)) => t_args(ns, key, args),
        None => {
            debug_assert!(false, "mt-i18n: 文案路径缺命名空间：{path}");
            path.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// 插值
// ---------------------------------------------------------------------------

/// 把 `{name}` 占位符替换成 `args` 中同名的值。
///
/// 语义严格对齐 TS 侧 `interpolate()`（正则 `/\{(\w+)\}/g`）：
/// - 占位符名只认 `[0-9A-Za-z_]`；
/// - **args 里没有的占位符原样保留**（不清空、不报错）——文案先落地、参数后补的
///   过渡期里，界面上看到的是 `{count}` 而不是一片空白，问题一眼可见；
/// - 不支持嵌套、不支持转义 `{{`（TS 侧同样不支持，字典里也没有这种写法）。
pub fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    if args.is_empty() || !template.contains('{') {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len() + 16);
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(rel) = template[i + 1..].find('}')
        {
            let name = &template[i + 1..i + 1 + rel];
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                if let Some((_, v)) = args.iter().find(|(k, _)| *k == name) {
                    out.push_str(v);
                } else {
                    // 未提供的占位符原样保留，与 TS 侧一致
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
                i += rel + 2;
                continue;
            }
        }
        // 非占位符：按 UTF-8 字符整体拷贝，避免切碎多字节序列
        let ch = template[i..].chars().next().expect("i 落在字符边界上");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 翻译宏：把插值参数写成 `name = value`，值只要实现 `Display` 即可。
///
/// ```
/// use mt_i18n::{tr, set_locale, Locale};
/// set_locale(Locale::Zh);
///
/// assert_eq!(tr!("app", "menu.settings"), "设置");          // 无插值：返回 &'static str
/// assert_eq!(tr!("time", "minutesAgo", n = 5), "5 分钟前"); // 有插值：返回 String
/// assert_eq!(tr!("app.menu.settings"), "设置");              // 单参 = 点分全路径
/// ```
#[macro_export]
macro_rules! tr {
    ($path:expr $(,)?) => {
        $crate::t_path($path)
    };
    ($ns:expr, $key:expr $(,)?) => {
        $crate::t($ns, $key)
    };
    ($ns:expr, $key:expr, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::t_args(
            $ns,
            $key,
            &[$((
                ::core::stringify!($name),
                &::std::string::ToString::to_string(&$value)[..],
            )),+],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 全局语言是进程级共享状态，而 cargo test 默认多线程并行跑。
    /// 所有会读写全局语言的用例都先抢这把锁，否则互相踩成 flaky。
    static GLOBAL_LOCALE_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn locale_code_roundtrip() {
        for l in Locale::ALL {
            assert_eq!(Locale::from_code(l.code()), Some(l));
            assert_eq!(l.to_string(), l.code());
        }
        assert_eq!(Locale::from_code("ZH"), Some(Locale::Zh));
        assert_eq!(Locale::from_code("fr"), None);
        assert!("fr".parse::<Locale>().is_err());
    }

    #[test]
    fn system_tag_detection() {
        assert_eq!(Locale::from_system_tag("zh-CN"), Locale::Zh);
        assert_eq!(Locale::from_system_tag("zh_TW.UTF-8"), Locale::Zh);
        assert_eq!(
            Locale::from_system_tag("Chinese (Simplified)_China.936"),
            Locale::Zh
        );
        assert_eq!(Locale::from_system_tag("en-US"), Locale::En);
        assert_eq!(Locale::from_system_tag("ja-JP"), Locale::En);
    }

    #[test]
    fn lookup_both_languages() {
        assert_eq!(t_in(Locale::Zh, "app", "menu.settings"), "设置");
        assert_eq!(t_in(Locale::En, "app", "menu.settings"), "Settings");
        assert_eq!(
            t_in(Locale::Zh, "app", "titleBar.status.idle"),
            "没有正在运行的 AI 会话"
        );
    }

    #[test]
    fn dynamic_lookup_is_silent() {
        // lookup 不断言不 panic：动态拼 key 的场景靠它
        assert!(lookup(Locale::Zh, "app", "titleBar.status.working").is_some());
        assert!(lookup(Locale::Zh, "app", "titleBar.status.nonexistent").is_none());
        assert!(lookup(Locale::Zh, "no_such_ns", "whatever").is_none());
    }

    #[test]
    fn interpolation_matches_ts_semantics() {
        assert_eq!(interpolate("{n} 分钟前", &[("n", "5")]), "5 分钟前");
        // 未提供的占位符原样保留
        assert_eq!(interpolate("{a} 和 {b}", &[("a", "1")]), "1 和 {b}");
        // 非 \w 内容不当占位符
        assert_eq!(interpolate("{a b}", &[("a b", "x")]), "{a b}");
        assert_eq!(interpolate("没有占位符", &[("a", "1")]), "没有占位符");
        // 多字节字符不会被切碎
        assert_eq!(interpolate("中文{x}中文", &[("x", "★")]), "中文★中文");
        // 同名占位符出现多次时全部替换
        assert_eq!(interpolate("{x}-{x}", &[("x", "9")]), "9-9");
    }

    #[test]
    fn tr_macro_forms() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_locale(Locale::Zh);
        assert_eq!(tr!("app", "menu.settings"), "设置");
        assert_eq!(tr!("app.menu.settings"), "设置");
        assert_eq!(tr!("time", "minutesAgo", n = 5), "5 分钟前");
        assert_eq!(
            tr!("app", "update.badge", version = "0.14.0"),
            "新版本 0.14.0"
        );
    }

    #[test]
    fn global_locale_switch_and_observer() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        static SEEN: AtomicU8 = AtomicU8::new(255);
        fn observe(l: Locale) {
            SEEN.store(l.as_u8(), Ordering::Relaxed);
        }
        add_locale_observer(observe);

        set_locale(Locale::En);
        assert_eq!(locale(), Locale::En);
        assert_eq!(SEEN.load(Ordering::Relaxed), Locale::En.as_u8());
        // 同值再切一次不算变化
        assert!(!set_locale(Locale::En));

        assert!(set_locale_from_code("zh"));
        assert_eq!(locale(), Locale::Zh);
        assert!(!set_locale_from_code("fr"));
        assert_eq!(locale(), Locale::Zh);
    }
}

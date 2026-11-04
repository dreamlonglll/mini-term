//! 便携 ConPTY 预载。从 `src-tauri/src/conpty_bootstrap.rs` 原样搬入,去 Tauri 化。
//!
//! # 为什么需要它
//!
//! Windows 系统自带的 ConPTY(`conhost`)在老版本上有一批已知缺陷(宽字符列宽、
//! 换行回绕、resize 丢行)。发行包里随附一份 Windows Terminal 的便携 ConPTY
//! (`portable-conpty/conpty.dll` + `x64|arm64/OpenConsole.exe`),启动时用
//! `LoadLibraryW` 以**绝对路径**预载。之后 portable-pty 内部用裸名
//! `LoadLibrary("conpty.dll")` 时会命中这个已加载模块 —— 全程不改进程 PATH。
//!
//! # ⚠️ 调用时机
//!
//! [`initialize`] **必须早于本 crate 的任何 [`crate::PtySession::spawn`]**
//! (即任何 `openpty`)。一旦 portable-pty 先用系统 ConPTY 建过 pty,
//! 后续预载就不再影响已解析的模块,便携后端形同虚设。
//! mt-app 在 `main()` 里、gpui 平台层建出来之前同步调用一次(时序依据见那里的注释)。
//!
//! 预检失败(文件缺失 / PE 架构不匹配 / 缺导出符号)一律回落系统 ConPTY,
//! 只打日志不报错 —— 便携后端是增强项,不是启动前置条件。
//!
//! # 开关
//!
//! 环境变量 [`DISABLE_ENV`]`=1` 时 [`initialize_default`] 跳过预载、直接回落系统
//! ConPTY(决策照常打日志)。给真机开 / 关对比和用户排障用:怀疑显示问题出在便携
//! 后端时,不必删文件就能换回系统 conhost。
//!
//! # ⚠️ 便携后端要求终端应答 DA1
//!
//! 便携 ConPTY(Windows Terminal 1.24)一起来就发 `ESC [ c`(Primary Device Attributes
//! 查询),**收到应答之前后续输出会卡住**(实测:不应答时 `ping -n 3` 只出来头两行,
//! 应答后全部到齐;系统 conhost 不发这个查询)。mini-term 里由 alacritty 的 `PtyWrite`
//! 事件自动应答(mt-app `pane.rs` 的 `drain_term_events`)。任何绕开 VT 状态机直接消费
//! PTY 输出的新调用方都得自己回 `ESC [ ? 6 c` 一类的应答,否则只看得到开头几行。

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const PORTABLE_CONPTY_DIR: &str = "portable-conpty";

/// 跳过便携预载、回落系统 ConPTY 的环境变量。取值 `1` 生效,其它一律按未设处理
/// (与 `MT_LOG_FILE` 同一口径)。
pub const DISABLE_ENV: &str = "MT_DISABLE_PORTABLE_CONPTY";

/// [`DISABLE_ENV`] 的取值是否要求跳过预载(纯函数,便于单测)。
fn disabled_by_env(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}
const PE_MACHINE_X64: u16 = 0x8664;
const PE_MACHINE_ARM64: u16 = 0xaa64;

const REQUIRED_X64_RESOURCES: [(&str, u16); 3] = [
    ("conpty.dll", PE_MACHINE_X64),
    ("x64/OpenConsole.exe", PE_MACHINE_X64),
    ("arm64/OpenConsole.exe", PE_MACHINE_ARM64),
];

/// 本次启动最终使用哪套 ConPTY 后端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConptyBootstrapDecision {
    /// 便携后端已预载,目录为资源根下的 `portable-conpty`。
    Portable { directory: PathBuf },
    /// 回落系统 ConPTY,`reason` 是可直接打日志的中文原因。
    System { reason: String },
}

fn system_decision(reason: impl Into<String>) -> ConptyBootstrapDecision {
    ConptyBootstrapDecision::System {
        reason: reason.into(),
    }
}

/// 读 PE 头里的 machine 字段。只读 DOS 头与 PE 签名那几个字节:这一步跑在启动
/// 关键路径上(主线程、窗口出来之前),三个文件合计几 MB,整份读进来纯属浪费。
fn read_pe_machine(path: &Path) -> Result<u16, String> {
    let mut file = fs::File::open(path).map_err(|error| format!("读取失败：{error}"))?;
    let mut dos = [0_u8; 0x40];
    if file.read_exact(&mut dos).is_err() || u16::from_le_bytes([dos[0], dos[1]]) != 0x5a4d {
        return Err("不是合法 PE 文件（缺少 MZ）".to_string());
    }
    let pe_offset = u32::from_le_bytes([dos[0x3c], dos[0x3d], dos[0x3e], dos[0x3f]]);
    let mut header = [0_u8; 6];
    if file.seek(SeekFrom::Start(u64::from(pe_offset))).is_err()
        || file.read_exact(&mut header).is_err()
        || header[..4] != *b"PE\0\0"
    {
        return Err("不是合法 PE 文件（缺少 PE header）".to_string());
    }
    Ok(u16::from_le_bytes([header[4], header[5]]))
}

fn validate_x64_resource_tree(portable_dir: &Path) -> Result<(), String> {
    for (relative, expected_machine) in REQUIRED_X64_RESOURCES {
        let path = portable_dir.join(relative);
        let machine =
            read_pe_machine(&path).map_err(|error| format!("{relative} 不可用：{error}"))?;
        if machine != expected_machine {
            return Err(format!(
                "{relative} PE machine 不匹配：expected=0x{expected_machine:04x} actual=0x{machine:04x}"
            ));
        }
    }
    Ok(())
}

/// 纯决策层:校验资源树 + 调用 `probe` 做实际预载,返回最终后端选择。
///
/// 与平台无关、不碰全局状态,便于单测覆盖每条回落分支。
pub fn choose_conpty_bootstrap<F>(
    resource_dir: &Path,
    target_arch: &str,
    probe: F,
) -> ConptyBootstrapDecision
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    if target_arch != "x86_64" {
        return system_decision(format!(
            "不支持的进程架构 {target_arch}；当前仅发布 Windows x64"
        ));
    }

    let portable_dir = resource_dir.join(PORTABLE_CONPTY_DIR);
    if let Err(error) = validate_x64_resource_tree(&portable_dir) {
        return system_decision(error);
    }
    if let Err(error) = probe(&portable_dir.join("conpty.dll")) {
        return system_decision(format!("conpty.dll 预检失败：{error}"));
    }

    ConptyBootstrapDecision::Portable {
        directory: portable_dir,
    }
}

fn log_decision(decision: &ConptyBootstrapDecision) {
    match decision {
        ConptyBootstrapDecision::Portable { directory, .. } => eprintln!(
            "[conpty-bootstrap] backend=portable arch={} dir={} dll=preloaded hosts=x64,arm64 fallback_boundary=preload-only",
            std::env::consts::ARCH,
            directory.display()
        ),
        ConptyBootstrapDecision::System { reason, .. } => eprintln!(
            "[conpty-bootstrap] backend=system arch={} reason={reason}",
            std::env::consts::ARCH
        ),
    }
}

/// 资源目录的默认推断:与可执行文件同目录。
///
/// Tauri 时代由 `app.path().resource_dir()` 给出,GPUI 侧还没有对应概念;
/// 打包脚本把 `portable-conpty/` 放在 exe 旁边即可,开发态则是
/// `target/debug/`(没有该目录时自然回落系统 ConPTY)。
pub fn default_resource_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

#[cfg(windows)]
mod windows_runtime {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::OnceLock;

    type ModuleHandle = *mut std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(file_name: *const u16) -> ModuleHandle;
        fn GetProcAddress(module: ModuleHandle, proc_name: *const u8) -> *mut std::ffi::c_void;
        fn FreeLibrary(module: ModuleHandle) -> i32;
    }

    // initialize 由应用启动路径调用一次；OnceLock 仍把这个进程级副作用做成
    // 并发安全、可重复调用的边界，避免未来新增入口时重复 LoadLibrary。
    static BOOTSTRAP_DECISION: OnceLock<ConptyBootstrapDecision> = OnceLock::new();
    // 保存由 LoadLibraryW 增加的 module 引用。进程退出前绝不 FreeLibrary，确保
    // portable-pty 随后的 LoadLibraryW("conpty.dll") 命中同一个已加载模块。
    static PRELOADED_MODULE: OnceLock<usize> = OnceLock::new();

    fn probe_and_preload(dll_path: &Path) -> Result<(), String> {
        let wide_path: Vec<u16> = dll_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let module = unsafe { LoadLibraryW(wide_path.as_ptr()) };
        if module.is_null() {
            return Err(format!(
                "LoadLibraryW({}) 失败：{}",
                dll_path.display(),
                std::io::Error::last_os_error()
            ));
        }

        for symbol in [
            b"CreatePseudoConsole\0".as_slice(),
            b"ResizePseudoConsole\0".as_slice(),
            b"ClosePseudoConsole\0".as_slice(),
        ] {
            if unsafe { GetProcAddress(module, symbol.as_ptr()) }.is_null() {
                unsafe {
                    FreeLibrary(module);
                }
                let symbol = String::from_utf8_lossy(&symbol[..symbol.len() - 1]);
                return Err(format!("缺少兼容导出 {symbol}"));
            }
        }

        if PRELOADED_MODULE.set(module as usize).is_err() {
            unsafe {
                FreeLibrary(module);
            }
            return Err("便携 ConPTY 模块已被预载".to_string());
        }

        // 不修改进程 PATH。Windows 的 LoadLibrary 搜索会先检查已加载模块，所以上面
        // 的绝对路径预载已经足以让 portable-pty 的裸名加载命中该 DLL；保留 module
        // 引用则让这个保证持续到进程退出。预检失败会在上面 FreeLibrary，系统 PATH
        // 也从未改变，portable-pty 因而继续使用原有系统回退路径。
        Ok(())
    }

    /// 预载便携 ConPTY。**必须早于任何 `openpty`**(见模块文档)。
    /// 幂等:重复调用返回首次的决策,不会重复 LoadLibrary。
    pub fn initialize(resource_dir: &Path) -> ConptyBootstrapDecision {
        BOOTSTRAP_DECISION
            .get_or_init(|| {
                let decision = choose_conpty_bootstrap(
                    resource_dir,
                    std::env::consts::ARCH,
                    probe_and_preload,
                );
                log_decision(&decision);
                decision
            })
            .clone()
    }

    /// 便携后端已经生效。没调过 [`initialize`](含 [`super::DISABLE_ENV`] 跳过预载)= 否。
    pub fn portable_active() -> bool {
        matches!(
            BOOTSTRAP_DECISION.get(),
            Some(ConptyBootstrapDecision::Portable { .. })
        )
    }
}

#[cfg(windows)]
pub use windows_runtime::{initialize, portable_active};

/// 非 Windows 平台没有 ConPTY,直接返回系统后端 —— 让调用方无需写 `cfg`。
#[cfg(not(windows))]
pub fn initialize(_resource_dir: &Path) -> ConptyBootstrapDecision {
    system_decision("非 Windows 平台不使用 ConPTY")
}

/// 非 Windows 平台没有 ConPTY,自然也没有便携后端。
#[cfg(not(windows))]
pub fn portable_active() -> bool {
    false
}

/// [`initialize`] + [`default_resource_dir`] 的组合:应用启动处一行调用。
/// [`DISABLE_ENV`]`=1` 时不预载,直接回落系统 ConPTY。
pub fn initialize_default() -> ConptyBootstrapDecision {
    if disabled_by_env(std::env::var_os(DISABLE_ENV).as_deref()) {
        let decision = system_decision(format!("{DISABLE_ENV}=1，按要求跳过便携预载"));
        log_decision(&decision);
        return decision;
    }
    match default_resource_dir() {
        Some(dir) => initialize(&dir),
        None => {
            let decision = system_decision("无法定位可执行文件所在目录");
            log_decision(&decision);
            decision
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            // 光靠时间戳不够唯一：cargo 并行跑测试，Windows 上两个线程同一刻取到的
            // 纳秒数会相等，撞名的两个用例就共用一份 portable-conpty 互相污染
            // （一个删 arm64、另一个把 x64 写成别的架构，断言随机错位）。补一个进程内
            // 自增序号兜底。
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "mini-term-conpty-{}-{nonce}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_pe(path: &Path, machine: u16) {
        let mut bytes = vec![0_u8; 0x100];
        bytes[0..2].copy_from_slice(&0x5a4d_u16.to_le_bytes());
        bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn complete_resources() -> TempDir {
        let temp = TempDir::new();
        let root = temp.path().join("portable-conpty");
        write_pe(&root.join("conpty.dll"), 0x8664);
        write_pe(&root.join("x64/OpenConsole.exe"), 0x8664);
        write_pe(&root.join("arm64/OpenConsole.exe"), 0xaa64);
        temp
    }

    #[test]
    fn complete_x64_resources_and_successful_probe_choose_portable() {
        let temp = complete_resources();
        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));

        match decision {
            ConptyBootstrapDecision::Portable { directory } => {
                assert_eq!(directory, temp.path().join("portable-conpty"));
            }
            other => panic!("expected portable decision, got {other:?}"),
        }
    }

    #[test]
    fn portable_decision_never_mutates_process_path() {
        let temp = complete_resources();
        let old_path = std::env::var_os("PATH");

        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));

        assert!(matches!(decision, ConptyBootstrapDecision::Portable { .. }));
        assert_eq!(std::env::var_os("PATH"), old_path);
    }

    #[test]
    fn missing_dll_chooses_system() {
        let temp = complete_resources();
        fs::remove_file(temp.path().join("portable-conpty/conpty.dll")).unwrap();

        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));

        assert_system(decision, "conpty.dll");
    }

    #[test]
    fn missing_required_host_chooses_system() {
        let temp = complete_resources();
        fs::remove_file(temp.path().join("portable-conpty/arm64/OpenConsole.exe")).unwrap();

        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));

        assert_system(decision, "arm64/OpenConsole.exe");
    }

    #[test]
    fn wrong_pe_architecture_chooses_system() {
        let temp = complete_resources();
        write_pe(
            &temp.path().join("portable-conpty/x64/OpenConsole.exe"),
            0xaa64,
        );

        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));

        assert_system(decision, "PE machine");
    }

    #[test]
    fn probe_failure_chooses_system() {
        let temp = complete_resources();

        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| {
            Err("缺少 CreatePseudoConsole 导出".to_string())
        });

        assert_system(decision, "CreatePseudoConsole");
    }

    /// PE 头只读前几个字节:文件短于 DOS 头、或 PE 偏移指到文件尾之外,都要按
    /// 「不是合法 PE」回落,而不是越界 / panic。
    #[test]
    fn truncated_pe_headers_choose_system() {
        let temp = complete_resources();
        let dll = temp.path().join("portable-conpty/conpty.dll");

        fs::write(&dll, b"MZ").unwrap();
        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));
        assert_system(decision, "缺少 MZ");

        let mut bytes = vec![0_u8; 0x40];
        bytes[0..2].copy_from_slice(&0x5a4d_u16.to_le_bytes());
        bytes[0x3c..0x40].copy_from_slice(&0x1000_u32.to_le_bytes());
        fs::write(&dll, bytes).unwrap();
        let decision = choose_conpty_bootstrap(temp.path(), "x86_64", |_| Ok(()));
        assert_system(decision, "缺少 PE header");
    }

    /// 开关只认 `1`:别的写法一律当没设,免得 `=0` 反而把便携后端关掉。
    #[test]
    fn disable_env_only_accepts_one() {
        use std::ffi::OsStr;
        assert!(disabled_by_env(Some(OsStr::new("1"))));
        for value in ["0", "", "true", " 1"] {
            assert!(!disabled_by_env(Some(OsStr::new(value))), "{value:?}");
        }
        assert!(!disabled_by_env(None));
    }

    #[test]
    fn unsupported_process_architecture_never_silently_selects_x64() {
        let temp = complete_resources();

        for arch in ["x86", "aarch64", "mips64"] {
            let decision = choose_conpty_bootstrap(temp.path(), arch, |_| Ok(()));
            assert_system(decision, "不支持的进程架构");
        }
    }

    fn assert_system(decision: ConptyBootstrapDecision, reason: &str) {
        match decision {
            ConptyBootstrapDecision::System { reason: actual } => {
                assert!(actual.contains(reason), "actual reason: {actual}");
            }
            other => panic!("expected system decision, got {other:?}"),
        }
    }
}

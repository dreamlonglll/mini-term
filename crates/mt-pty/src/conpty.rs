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
//! 惯例是在 `main()` 里创建窗口之前调用一次。
//!
//! 预检失败(文件缺失 / PE 架构不匹配 / 缺导出符号)一律回落系统 ConPTY,
//! 只打日志不报错 —— 便携后端是增强项,不是启动前置条件。

use std::fs;
use std::path::{Path, PathBuf};

const PORTABLE_CONPTY_DIR: &str = "portable-conpty";
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

fn read_pe_machine(path: &Path) -> Result<u16, String> {
    let bytes = fs::read(path).map_err(|error| format!("读取失败：{error}"))?;
    if bytes.len() < 0x40 || u16::from_le_bytes([bytes[0], bytes[1]]) != 0x5a4d {
        return Err("不是合法 PE 文件（缺少 MZ）".to_string());
    }
    let pe_offset =
        u32::from_le_bytes([bytes[0x3c], bytes[0x3d], bytes[0x3e], bytes[0x3f]]) as usize;
    if pe_offset.checked_add(6).is_none_or(|end| end > bytes.len())
        || bytes[pe_offset..pe_offset + 4] != *b"PE\0\0"
    {
        return Err("不是合法 PE 文件（缺少 PE header）".to_string());
    }
    Ok(u16::from_le_bytes([
        bytes[pe_offset + 4],
        bytes[pe_offset + 5],
    ]))
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
}

#[cfg(windows)]
pub use windows_runtime::initialize;

/// 非 Windows 平台没有 ConPTY,直接返回系统后端 —— 让调用方无需写 `cfg`。
#[cfg(not(windows))]
pub fn initialize(_resource_dir: &Path) -> ConptyBootstrapDecision {
    system_decision("非 Windows 平台不使用 ConPTY")
}

/// [`initialize`] + [`default_resource_dir`] 的组合:应用启动处一行调用。
pub fn initialize_default() -> ConptyBootstrapDecision {
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
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("mini-term-conpty-{}-{nonce}", std::process::id()));
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

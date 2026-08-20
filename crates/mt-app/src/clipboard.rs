//! 长文本粘贴转文件(audit #30 的主体)。
//!
//! 对应 `src/utils/terminalCache.ts:670-774` 的阈值判定与粘贴主流程、
//! `src/utils/pastePath.ts` 的三类 pane 路径映射,以及
//! `src-tauri/src/clipboard.rs` 的临时文件落盘与 24h 清理。
//!
//! # 为什么落在 mt-app 而不是 mt-ui
//!
//! 阈值判定要读 [`AppConfig`](mt_config::AppConfig)、路径映射要知道项目是不是
//! WSL / 远程 —— 那都是壳的东西。`mt_ui::TerminalView` 只留一个
//! [`on_paste`](mt_ui::TerminalView::on_paste) 钩子,内建的 `paste()` 依旧纯粹。
//!
//! # 本批的一处收窄(相对原版)
//!
//! **剪贴板图片不做**。原版会先问 `clipboardHasImage()`,读 Win32 DIB 存成
//! 临时 PNG、粘路径,读不到则退 `Alt+V`。那套(`clipboard.rs` 的 `parse_dib`
//! 含加固与单测)是另一个缺口,audit #30 的措辞里没有它。
//!
//! ~~SSH 远程分支不做~~ ✅ BB-a 批补上:见 [`spawn_remote_paste`]。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::notify::ToastKind;
use crate::store::AppStore;
use crate::toast;
use crate::tr;

/// 临时文件目录名。与装机版**共用同一个目录名**(图片与文本同处),
/// 于是两边的 24h 清理互相覆盖得到。
const TEMP_DIR: &str = "mini-term-clipboard";

/// 清理阈值:24 小时。
const CLEANUP_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// 判定剪贴板文本是否要转存为临时文件。
///
/// 逐字照抄 `terminalCache.ts:671-678`:**任一阈值命中即转存**,
/// 阈值为 0 表示该维度不判;比较是 `>=` 而不是 `>`。
///
/// ⚠️ 字符数取 **UTF-16 码元数**而不是 `chars().count()` —— JS 的 `String.length`
/// 就是码元数,中文按 1、emoji 按 2。用 `chars()` 会让同一段文本在两个版本里
/// 判定不同(emoji 多的文本尤其明显)。
pub fn is_long_text(text: &str, line_threshold: u32, char_threshold: u32) -> bool {
    if char_threshold > 0 && text.encode_utf16().count() >= char_threshold as usize {
        return true;
    }
    if line_threshold > 0 {
        // CRLF 先归一再按 \n 切,与 `text.replace(/\r\n/g,'\n').split('\n').length` 同
        let lines = text.replace("\r\n", "\n").split('\n').count();
        if lines >= line_threshold as usize {
            return true;
        }
    }
    false
}

/// 临时文件目录(`std::env::temp_dir()/mini-term-clipboard`)。
fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(TEMP_DIR)
}

/// 把长文本写进临时 `.txt`,返回绝对路径。
///
/// 文件名 `paste-{unix_millis}.txt`,与装机版 `save_clipboard_text` 一字不差
/// (`src-tauri/src/clipboard.rs:321-327`)—— 两个版本轮流跑时清理逻辑仍然通用。
/// 错误文案同样照抄(装机版这两句就是硬编码中文,不走 i18n)。
pub fn save_clipboard_text(text: &str) -> Result<PathBuf, String> {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let path = dir.join(format!("paste-{}.txt", unix_millis()));
    std::fs::write(&path, text.as_bytes()).map_err(|e| format!("写入临时文件失败: {e}"))?;
    Ok(path)
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// 删掉临时目录里 mtime 超过 24h 的**全部**文件。启动时调一次。
///
/// 与装机版 `cleanup_old_clipboard_images` 同语义:粘进终端的路径用完就没人管了,
/// 不清的话这个目录会随使用无界增长。目录不存在直接返回(还没粘过)。
pub fn cleanup_old_files() {
    let Ok(entries) = std::fs::read_dir(temp_dir()) else {
        return;
    };
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(CLEANUP_AGE) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Windows 盘符路径 → WSL 内可读路径(`C:\a\b.txt` → `/mnt/c/a/b.txt`)。
///
/// 照抄 `src/utils/wslPath.ts::windowsPathToWsl`。只处理盘符路径(含 `\\?\`
/// verbatim 前缀);UNC / 已是 POSIX 形式的返回 `None`,调用方按原样粘贴。
///
/// 已知边界(原版同款):`/mnt` 是 automount 的默认挂载点,用户在
/// `/etc/wsl.conf` 里改过 `[automount] root=` 时不成立 —— 表现是「文件不存在」,
/// 不会误写。
pub fn windows_path_to_wsl(path: &str) -> Option<String> {
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut chars = stripped.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') {
        return None;
    }
    let sep = chars.next()?;
    if sep != '\\' && sep != '/' {
        return None;
    }
    let rest: String = chars.collect::<String>().replace('\\', "/");
    Some(format!("/mnt/{}/{rest}", drive.to_ascii_lowercase()))
}

/// 这个 pane 落盘的文件该以什么路径粘进去。
///
/// 判定口径刻意与后端起 PTY 的分支一致(`pastePath.ts:13-18` 原注释):
/// 判错的代价不对称 —— 漏判只是回到改动前的行为,误判会把路径指向另一台机器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteTarget {
    /// 本地 shell:原样粘 Windows 路径。
    Local,
    /// WSL:转 `/mnt/<盘符>/...`(文件本身经 automount 就能读到,只差路径形式)。
    Wsl,
    /// SSH 远程项目:转存成本机临时文件后 **SFTP 传到远端**,粘远端绝对路径
    /// (`ssh_remote_upload_paste`)。这一支是异步的,走 [`spawn_remote_paste`],
    /// 不经 [`map_pasted_path`]。
    Ssh,
}

/// pane 用的 shell 是不是 `wsl.exe`(本地项目里手工配了 WSL shell 的情况)。
///
/// 取命令的 **basename** 再比对:既不漏判 `C:\Windows\System32\wsl.exe`,
/// 也不误判 `wslconfig.exe` 这类同前缀命令(`pastePath.ts:46-49` 的同一条注释)。
pub fn command_is_wsl(command: &str) -> bool {
    let base = command
        .trim()
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    base == "wsl" || base == "wsl.exe"
}

/// 判断某个 pty 所在 pane 的粘贴目标(`pastePath.ts::resolvePasteTarget`)。
/// 定位不到 pane / 项目时退回 [`PasteTarget::Local`](原版同款兜底)。
pub fn resolve_paste_target(store: &AppStore, pty_id: u32) -> PasteTarget {
    let Some((project_id, pane_id)) = store.pane_of_pty(pty_id) else {
        return PasteTarget::Local;
    };
    let Some(project) = store.project(&project_id) else {
        return PasteTarget::Local;
    };
    if project.ssh_connection_id.is_some() {
        return PasteTarget::Ssh;
    }
    // 项目根是 WSL UNC → 后端起 PTY 时就已经改用 wsl.exe 了(decide_wsl_override)
    if mt_pty::decide_wsl_override(&project.path).is_some() {
        return PasteTarget::Wsl;
    }
    // 本地项目但 pane 自己配了 wsl.exe 当 shell
    let shell_name = store
        .project_state(&project_id)
        .and_then(|s| s.layout.as_ref())
        .and_then(|l| l.pane(&pane_id))
        .map(|p| p.shell_name.clone());
    let runs_wsl = shell_name
        .and_then(|name| {
            store
                .config()
                .available_shells
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.command.clone())
        })
        .is_some_and(|cmd| command_is_wsl(&cmd));
    if runs_wsl {
        PasteTarget::Wsl
    } else {
        PasteTarget::Local
    }
}

/// 把本机临时文件路径映射成「该终端里真正可读的路径」。
///
/// [`PasteTarget::Ssh`] 走不到这里 —— 那一支是异步上传,见 [`spawn_remote_paste`]。
pub fn map_pasted_path(local: &Path, target: PasteTarget) -> String {
    let local = local.to_string_lossy().into_owned();
    match target {
        PasteTarget::Local | PasteTarget::Ssh => local,
        // 转不了(UNC 等非盘符路径)就原样返回,行为退回改动前
        PasteTarget::Wsl => windows_path_to_wsl(&local).unwrap_or(local),
    }
}

// ---------------------------------------------------------------------------
// SSH 远程 pane 的粘贴(异步:转存 → SFTP 上传 → 写远端路径)
// ---------------------------------------------------------------------------

/// 正在处理粘贴的 pty(`terminalCache.ts:680` 的 `pasteInFlight`)。
///
/// 远程上传要几百毫秒到几秒,这期间用户连按 Ctrl+V 会让多条路径以完成顺序
/// **乱序**插进命令行;直接丢弃重入的那次,语义上等同「还没粘完」。
static PASTE_IN_FLIGHT: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn in_flight_begin(pty_id: u32) -> bool {
    let mut guard = PASTE_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashSet::new).insert(pty_id)
}

fn in_flight_end(pty_id: u32) {
    let mut guard = PASTE_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = guard.as_mut() {
        set.remove(&pty_id);
    }
}

/// 远程 pane 的长文本粘贴:后台转存 + SFTP 上传,完成后把**远端绝对路径**
/// 写进该 pane。
///
/// # 为什么必须异步
///
/// `TerminalView` 的粘贴钩子是**同步返回值制**([`mt_ui::PasteAction`] 的注释:
/// 宿主在钩子里回头 update 视图 = 同一实体嵌套 update,gpui 当场 panic),
/// 而上传要跨网络。于是钩子当场返回 `PasteAction::None`(语义就是「宿主已接管」),
/// 真正的写入由这条任务在完成后经 `AppStore::write_to_pane` 补上。
///
/// # 失败面(与原版逐条对齐)
///
/// - 断链 / 上传失败 / 转存失败 → 弹一条 `paste-error` toast,**并把原文粘进去**
///   (`notifyPasteFailure` 之后继续走 `enqueuePtyWrite(text)` 的那一支:
///   就是长了点,比什么都没有强);
/// - pane 在上传期间被关掉 → `write_to_pane` 返回 false,什么都不发生。
pub fn spawn_remote_paste(
    pty_id: u32,
    text: String,
    connection: mt_config::SshConnection,
    project_path: String,
    project_id: String,
    project_name: String,
    dest_dir: String,
    cx: &mut gpui::App,
) {
    if !in_flight_begin(pty_id) {
        return; // 上一次还没粘完,丢弃这次(原版同款)
    }
    let store = AppStore::global(cx);
    cx.spawn(async move |cx| {
        let payload = text.clone();
        let uploaded = cx
            .background_executor()
            .spawn(async move {
                let local = save_clipboard_text(&payload)?;
                let local = local.to_string_lossy().into_owned();
                crate::remote_ssh::upload_paste(&connection, &project_path, &local, &dest_dir)
            })
            .await;
        in_flight_end(pty_id);

        let _ = store.update(cx, |store, cx| {
            let Some((project, pane)) = store.pane_of_pty(pty_id) else {
                return; // pane 在上传期间被关掉了
            };
            match uploaded {
                Ok(remote_path) => {
                    store.write_to_pane(&project, &pane, &quote_path(&remote_path), cx);
                }
                Err(detail) => {
                    eprintln!("[pane {pty_id}] 粘贴内容上传到远端失败: {detail}");
                    toast::push_message(
                        ToastKind::PasteError,
                        project_id,
                        project_name,
                        tr!("terminal", "pasteUploadFailed", detail = detail),
                        cx,
                    );
                    // 提示完继续粘原文 —— 与原版一致
                    store.write_to_pane(&project, &pane, &text, cx);
                }
            }
        });
    })
    .detach();
}

/// 粘进终端的那一串:**带英文双引号**(兼容含空格的路径),
/// 不追加空格、不追加回车(`terminalCache.ts:757`)。
pub fn quote_path(path: &str) -> String {
    format!("\"{path}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字符阈值:`>=` 命中,0 = 该维度不判。
    #[test]
    fn 字符阈值按码元数且是闭区间() {
        assert!(is_long_text(&"a".repeat(2000), 0, 2000));
        assert!(!is_long_text(&"a".repeat(1999), 0, 2000));
        assert!(!is_long_text(&"a".repeat(99999), 0, 0), "0 = 不按字符判");
    }

    /// 行阈值:CRLF 归一后按 `\n` 切,行数 = 分段数(末尾无换行也算一行)。
    #[test]
    fn 行阈值归一_crlf_后计数() {
        // 9 个 \n → 10 行
        let ten = "x\r\n".repeat(9) + "x";
        assert!(is_long_text(&ten, 10, 0));
        let nine = "x\r\n".repeat(8) + "x";
        assert!(!is_long_text(&nine, 10, 0));
        // \r\n 不许被数成两次换行
        assert!(!is_long_text("a\r\nb", 3, 0));
        assert!(is_long_text("a\r\nb", 2, 0));
    }

    /// 任一阈值命中即转存(不是「都要满足」)。
    #[test]
    fn 任一阈值命中即为长文本() {
        // 只超字符数
        assert!(is_long_text(&"a".repeat(2000), 10, 2000));
        // 只超行数
        assert!(is_long_text(&"x\n".repeat(20), 10, 2000));
        // 都不超
        assert!(!is_long_text("hello", 10, 2000));
    }

    /// 两个阈值都是 0 = 整个功能哑掉(用户手动关掉两个维度)。
    #[test]
    fn 两个阈值都为零时永不命中() {
        assert!(!is_long_text(&"x\n".repeat(9999), 0, 0));
    }

    /// 字符数按 UTF-16 码元:emoji 算 2、中文算 1 —— 与 JS `String.length` 对齐。
    #[test]
    fn 字符数用_utf16_码元而非_char() {
        // 5 个 emoji = 10 个码元(chars().count() 只有 5)
        let emoji = "😀".repeat(5);
        assert_eq!(emoji.chars().count(), 5);
        assert!(is_long_text(&emoji, 0, 10));
        assert!(!is_long_text(&emoji, 0, 11));
        // 中文是 1 个码元
        assert!(is_long_text(&"中".repeat(10), 0, 10));
        assert!(!is_long_text(&"中".repeat(9), 0, 10));
    }

    /// 盘符路径 → `/mnt/<小写盘符>/...`,反斜杠一律转正斜杠。
    #[test]
    fn windows_路径转_wsl() {
        assert_eq!(
            windows_path_to_wsl(r"C:\Users\me\paste-1.txt").as_deref(),
            Some("/mnt/c/Users/me/paste-1.txt")
        );
        // verbatim 前缀要先剥掉
        assert_eq!(
            windows_path_to_wsl(r"\\?\D:\tmp\a.txt").as_deref(),
            Some("/mnt/d/tmp/a.txt")
        );
        // 正斜杠分隔同样认
        assert_eq!(
            windows_path_to_wsl("E:/tmp/a.txt").as_deref(),
            Some("/mnt/e/tmp/a.txt")
        );
    }

    /// 非盘符路径转不了 —— 返回 None,调用方原样粘。
    #[test]
    fn 非盘符路径不转换() {
        assert_eq!(windows_path_to_wsl(r"\\wsl$\Ubuntu\home\me\a.txt"), None);
        assert_eq!(windows_path_to_wsl("/home/me/a.txt"), None);
        assert_eq!(windows_path_to_wsl("relative/a.txt"), None);
        assert_eq!(windows_path_to_wsl("C:"), None, "缺分隔符");
        assert_eq!(windows_path_to_wsl(""), None);
    }

    /// shell 命令是不是 wsl:按 basename 比,不漏判全路径、不误判同前缀命令。
    #[test]
    fn wsl_shell_按_basename_判定() {
        assert!(command_is_wsl("wsl"));
        assert!(command_is_wsl("wsl.exe"));
        assert!(command_is_wsl(r"C:\Windows\System32\wsl.exe"));
        assert!(command_is_wsl(" WSL.EXE "), "大小写与空白都要吃掉");
        assert!(!command_is_wsl("wslconfig.exe"));
        assert!(!command_is_wsl("powershell.exe"));
        assert!(!command_is_wsl(""));
    }

    /// 路径映射:local 原样、wsl 转 /mnt、ssh 本批不转(走不到)。
    #[test]
    fn 路径按目标映射() {
        let p = Path::new(r"C:\tmp\paste-1.txt");
        assert_eq!(
            map_pasted_path(p, PasteTarget::Local),
            r"C:\tmp\paste-1.txt"
        );
        assert_eq!(
            map_pasted_path(p, PasteTarget::Wsl),
            "/mnt/c/tmp/paste-1.txt"
        );
        // 转不了的路径在 wsl 目标下原样返回
        let unc = Path::new(r"\\server\share\a.txt");
        assert_eq!(
            map_pasted_path(unc, PasteTarget::Wsl),
            r"\\server\share\a.txt"
        );
    }

    /// 粘进终端的是带双引号的路径,前后不加空格也不加回车。
    #[test]
    fn 粘贴串带双引号且不带回车() {
        assert_eq!(quote_path(r"C:\a b\c.txt"), "\"C:\\a b\\c.txt\"");
        assert!(!quote_path("x").contains('\r'));
        assert!(!quote_path("x").ends_with(' '));
    }

    /// 转存的文件名格式与装机版一字不差(两个版本共用同一个目录与清理逻辑)。
    #[test]
    fn 临时文件名与装机版同格式() {
        let dir = temp_dir();
        assert!(dir.ends_with(TEMP_DIR));
        let path = save_clipboard_text("hello").expect("写临时文件");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("paste-"), "{name}");
        assert!(name.ends_with(".txt"), "{name}");
        assert!(
            name["paste-".len()..name.len() - 4]
                .chars()
                .all(|c| c.is_ascii_digit()),
            "中间必须是纯毫秒数:{name}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let _ = std::fs::remove_file(&path);
    }
}

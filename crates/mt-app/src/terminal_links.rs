//! 终端里的文件链接:按住 Ctrl(macOS 上 ⌘)悬停 AI 输出里的文件路径出下划线,
//! 单击在文件页新开页签(带行号就定位到那一行)。
//!
//! mt-ui 只认形状(哪段字符像路径、带不带行号,见 `mt_terminal::links`),这里回答
//! 「它指向哪个文件、文件页能不能打开」:
//!
//! - **相对路径**依次对着 AI 会话的启动目录、pane 的工作目录、项目根解析,第一个
//!   在盘上的胜出 —— agent 报的路径相对它自己的 cwd,worktree pane 的 cwd 也不是项目根;
//! - **WSL**:WSL 项目把 `/home/u/proj/...` 换回项目根那条 UNC 路径;本地项目里
//!   `/mnt/d/...`(跑 wsl.exe 的 pane)与 Git Bash 的 `/d/...` 换回 `D:\...`;
//! - **只认项目根内的文件**:文件页读盘就是这把尺子(`mt_project::fs::read_file_content`),
//!   项目外的路径就算存在也打不开,不画下划线免得点了才报错;
//! - **SSH 远程项目**没法每次悬停都去 SFTP stat,只按形状判:项目根下的绝对路径,
//!   或带 `/` 的相对路径(对着项目根),末段还得像个文件。不存在由文件页自己报错。
//!
//! 文件页按**活动项目**开页签(`workbench_area::open_active_file`);看得见的终端都属于
//! 活动项目,不属于的一律不认。

use std::path::{Component, Path, PathBuf};

use gpui::{App, Window};
use mt_terminal::links::PathCandidate;
use mt_ui::terminal::FileLink;

use crate::clipboard::{self, PasteTarget};
use crate::file_viewer::LineTarget;
use crate::store::AppStore;

/// 解析路径要看的那一份项目形态。
enum Host {
    /// 本地项目。`wsl_shell` = 这个 pane 的 shell 是 wsl.exe(输出里是 `/mnt/d/...`)。
    Local { root: PathBuf, wsl_shell: bool },
    /// 项目根本身是 WSL UNC。`unix_root` 是同一目录在 WSL 里的写法。
    Wsl { root: PathBuf, unix_root: String },
    /// SSH 远程项目,`root` 是远程 POSIX 路径。
    Remote { root: String },
}

/// `pty_id` 那个终端里的一段文字指向哪个能在文件页打开的文件。
///
/// 在终端元素的 paint 期调用(结果由 mt-ui 按片段缓存),所以只读 store、只做 stat。
pub fn resolve(pty_id: u32, cand: &PathCandidate, cx: &App) -> Option<PathBuf> {
    let store = AppStore::global(cx);
    let s = store.read(cx);
    let (project_id, pane_id) = s.pane_of_pty(pty_id)?;
    if s.active_project_id.as_deref() != Some(project_id.as_str()) {
        return None;
    }
    let project = s.project(&project_id)?;
    let host = if s.is_remote_project(&project_id) {
        Host::Remote {
            root: project.path.clone(),
        }
    } else if let Some(wsl) = mt_core::parse_wsl_unc(&project.path) {
        Host::Wsl {
            root: PathBuf::from(&project.path),
            unix_root: wsl.unix_path,
        }
    } else {
        Host::Local {
            root: PathBuf::from(&project.path),
            wsl_shell: clipboard::resolve_paste_target(s, pty_id) == PasteTarget::Wsl,
        }
    };
    let pane = s
        .project_state(&project_id)
        .and_then(|st| st.pane(&pane_id));
    // 相对路径的基准:AI 会话启动目录在前(agent 报的路径相对它),再是 pane 工作目录
    let bases: Vec<&str> = pane
        .into_iter()
        .flat_map(|p| {
            [
                p.ai_session.as_ref().and_then(|ai| ai.cwd.as_deref()),
                p.cwd.as_deref(),
            ]
        })
        .flatten()
        .collect();
    match &host {
        Host::Remote { root } => remote_target(cand, root),
        Host::Local { root, .. } | Host::Wsl { root, .. } => {
            local_targets(&cand.path, &host, &bases)
                .into_iter()
                .find(|path| mt_project::fs::is_viewable_file(root, path))
        }
    }
}

/// Ctrl+单击:在文件页打开(已开过就切过去),定位到行;范围写法连同选中那几行。
pub fn open(link: &FileLink, window: &mut Window, cx: &mut App) {
    crate::workbench_area::open_active_file(
        AppStore::global(cx),
        link.path.clone(),
        link.line.map(|line| LineTarget {
            line,
            end: link.end_line,
        }),
        window,
        cx,
    );
}

/// 本地 / WSL 项目下,`raw` 依次可能是哪些文件(还没验盘)。
///
/// 绝对路径只有一种解释;认不出的绝对写法(Windows 上的 `/usr/...`、`~/...`)一个都不给,
/// 不拿它去拼基准目录。相对路径对每个基准目录各拼一次,项目根垫底,去重保序。
fn local_targets(raw: &str, host: &Host, bases: &[&str]) -> Vec<PathBuf> {
    if let Some(abs) = absolute_on_host(raw, host) {
        return vec![normalize(&abs)];
    }
    if looks_absolute(raw) {
        return Vec::new();
    }
    let root = match host {
        Host::Local { root, .. } | Host::Wsl { root, .. } => root,
        Host::Remote { .. } => return Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = bases
        .iter()
        .filter_map(|base| {
            absolute_on_host(base, host).or_else(|| {
                let path = PathBuf::from(base);
                path.is_absolute().then_some(path)
            })
        })
        .collect();
    dirs.push(root.clone());
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let candidate = normalize(&dir.join(raw));
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// 绝对路径换成本机能读的写法。不是绝对路径、或认不出来,返回 `None`。
fn absolute_on_host(raw: &str, host: &Host) -> Option<PathBuf> {
    if is_windows_absolute(raw) {
        return cfg!(windows).then(|| PathBuf::from(raw));
    }
    if !raw.starts_with('/') {
        return None;
    }
    match host {
        // 项目外的 Linux 路径文件页反正打不开,只换项目根下的
        Host::Wsl { root, unix_root } => {
            strip_dir_prefix(raw, unix_root).map(|rest| root.join(rest))
        }
        Host::Local { wsl_shell, .. } => {
            if cfg!(windows) {
                posix_drive_to_windows(raw, *wsl_shell)
            } else {
                Some(PathBuf::from(raw))
            }
        }
        Host::Remote { .. } => None,
    }
}

/// `/mnt/d/x` → `D:\x`(WSL 的 automount);`wsl_shell` 为假时再认 Git Bash 的 `/d/x`。
///
/// 后一种在 WSL 里不是盘符映射,所以跑 wsl.exe 的 pane 不认它。
fn posix_drive_to_windows(raw: &str, wsl_shell: bool) -> Option<PathBuf> {
    let after_mnt = raw.strip_prefix("/mnt/");
    let rest = match after_mnt {
        Some(rest) => rest,
        None if !wsl_shell => raw.strip_prefix('/')?,
        None => return None,
    };
    let mut chars = rest.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let tail = chars.as_str();
    if !(tail.is_empty() || tail.starts_with('/')) {
        return None;
    }
    let tail = tail.trim_start_matches('/').replace('/', "\\");
    Some(PathBuf::from(format!(
        "{}:\\{tail}",
        drive.to_ascii_uppercase()
    )))
}

/// `X:\` / `X:/` 起头,或 `\\` 起头的 UNC。
fn is_windows_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    raw.starts_with("\\\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// 看着像绝对路径(哪怕这台机器认不出来)。这类不该再拿去拼基准目录。
fn looks_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    raw.starts_with(['/', '\\', '~'])
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// `raw` 在 `dir` 底下时返回剩下的那截(不带前导 `/`;正好是 `dir` 本身时为空串)。
fn strip_dir_prefix<'a>(raw: &'a str, dir: &str) -> Option<&'a str> {
    let rest = raw.strip_prefix(dir.trim_end_matches('/'))?;
    if rest.is_empty() {
        Some(rest)
    } else {
        rest.strip_prefix('/')
    }
}

/// 字面上消掉 `.` / `..` 并统一分隔符(不碰盘)。
///
/// 文件页按规范化后的路径给页签去重(`DocumentKey`),`./src/a.ts` 与文件树点开的
/// `src\a.ts` 要落成同一个,不然同一个文件会开出两个页签。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// SSH 远程项目:不验盘,只按形状判(见模块注释)。
fn remote_target(cand: &PathCandidate, root: &str) -> Option<PathBuf> {
    let raw = cand.path.as_str();
    if raw.contains('\\') || raw.starts_with('~') || is_windows_absolute(raw) {
        return None;
    }
    let last = raw.rsplit('/').next().unwrap_or(raw);
    if !last.contains('.') && cand.line.is_none() {
        return None;
    }
    let root = root.trim_end_matches('/');
    let full = if raw.starts_with('/') {
        raw.to_string()
    } else if raw.contains('/') {
        format!("{root}/{raw}")
    } else {
        return None;
    };
    let full = normalize_posix(&full);
    strip_dir_prefix(&full, root)
        .filter(|rest| !rest.is_empty())
        .map(|_| PathBuf::from(full.clone()))
}

fn normalize_posix(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, line: Option<u32>) -> PathCandidate {
        PathCandidate {
            path: path.to_string(),
            line,
            end_line: None,
            column: None,
            range: 0..path.chars().count(),
        }
    }

    #[test]
    fn 远程项目只认项目根下像文件的路径() {
        let root = "/srv/app/";
        assert_eq!(
            remote_target(&cand("/srv/app/src/main.rs", Some(3)), root),
            Some(PathBuf::from("/srv/app/src/main.rs"))
        );
        assert_eq!(
            remote_target(&cand("./src/../lib/a.py", None), root),
            Some(PathBuf::from("/srv/app/lib/a.py")),
            "相对路径对着项目根,`.`/`..` 字面消掉"
        );
        assert_eq!(
            remote_target(&cand("/etc/passwd", Some(1)), root),
            None,
            "根外"
        );
        assert_eq!(
            remote_target(&cand("../../etc/x.conf", None), root),
            None,
            "爬出根外"
        );
        assert_eq!(
            remote_target(&cand("README.md", None), root),
            None,
            "不带 / 的相对名不认"
        );
        assert_eq!(
            remote_target(&cand("src/components", None), root),
            None,
            "末段不像文件"
        );
        assert_eq!(
            remote_target(&cand("src/Makefile", Some(9)), root),
            Some(PathBuf::from("/srv/app/src/Makefile")),
            "带行号就算末段没扩展名"
        );
    }

    #[test]
    fn 目录前缀按整段匹配() {
        assert_eq!(
            strip_dir_prefix("/home/u/proj/a.rs", "/home/u/proj"),
            Some("a.rs")
        );
        assert_eq!(strip_dir_prefix("/home/u/proj", "/home/u/proj/"), Some(""));
        assert_eq!(
            strip_dir_prefix("/home/u/project2/a.rs", "/home/u/proj"),
            None
        );
        assert_eq!(
            strip_dir_prefix("/a.rs", "/"),
            Some("a.rs"),
            "distro 根当项目"
        );
    }

    #[cfg(windows)]
    #[test]
    fn 本地相对路径按基准目录依次拼且去重() {
        let host = Host::Local {
            root: PathBuf::from(r"D:\proj"),
            wsl_shell: false,
        };
        let out = local_targets(
            "./src/a.ts",
            &host,
            &[r"D:\proj\.worktrees\feat", r"D:\proj"],
        );
        assert_eq!(
            out,
            vec![
                PathBuf::from(r"D:\proj\.worktrees\feat\src\a.ts"),
                PathBuf::from(r"D:\proj\src\a.ts"),
            ]
        );
        // 绝对路径只有一种解释,正斜杠统一成反斜杠
        assert_eq!(
            local_targets("D:/proj/src/../b.ts", &host, &[]),
            vec![PathBuf::from(r"D:\proj\b.ts")]
        );
        // 认不出的绝对写法不拿去拼基准目录
        assert!(local_targets("~/x.ts", &host, &[]).is_empty());
        assert!(
            local_targets(
                "/usr/lib/x.so",
                &Host::Local {
                    root: PathBuf::from(r"D:\proj"),
                    wsl_shell: true,
                },
                &[]
            )
            .is_empty()
        );
    }

    #[cfg(windows)]
    #[test]
    fn posix_盘符写法换回_windows() {
        assert_eq!(
            posix_drive_to_windows("/mnt/d/Git/x/a.ts", true),
            Some(PathBuf::from(r"D:\Git\x\a.ts"))
        );
        assert_eq!(
            posix_drive_to_windows("/d/Git/x/a.ts", false),
            Some(PathBuf::from(r"D:\Git\x\a.ts")),
            "Git Bash"
        );
        assert_eq!(
            posix_drive_to_windows("/d/Git/x/a.ts", true),
            None,
            "WSL 里 /d 不是盘符"
        );
        assert_eq!(
            posix_drive_to_windows("/data/x", false),
            None,
            "首段不止一个字母"
        );
    }

    #[cfg(windows)]
    #[test]
    fn wsl_项目把_linux_路径换回项目根的_unc() {
        let host = Host::Wsl {
            root: PathBuf::from(r"\\wsl.localhost\Ubuntu\home\u\proj"),
            unix_root: "/home/u/proj".into(),
        };
        assert_eq!(
            local_targets("/home/u/proj/src/main.rs", &host, &[]),
            vec![PathBuf::from(
                r"\\wsl.localhost\Ubuntu\home\u\proj\src\main.rs"
            )]
        );
        assert!(
            local_targets("/etc/hosts", &host, &[]).is_empty(),
            "项目外不换"
        );
        // AI 会话报上来的 cwd 是 Linux 写法,也要先换回 UNC 再拼
        assert_eq!(
            local_targets("a.rs", &host, &["/home/u/proj/crates/x"])[0],
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\u\proj\crates\x\a.rs")
        );
    }
}

//! 工具活动里「路径类」目标的缩短。
//!
//! 手机镜像的窄屏上,工具行的目标单行省略、展开后也只给几行:绝对路径一长,
//! 结尾的文件名(最关键的部分)就被截掉了,两条 Read 看起来一模一样。所以路径类
//! 目标先缩短再摘要。哪些目标算路径由 `tool_activity` 按工具逐个声明,本模块只管
//! 怎么缩:
//!
//! - 落在项目根之下 → 去掉根与紧跟的分隔符,成相对路径(`src\cart.js`),分隔符
//!   风格原样;就是项目根本身 → `.`;
//! - 不在项目根下、在用户家目录下 → 家目录前缀换成 `~`(`~\AppData\…`);
//! - 其余(别的盘、本来就是相对路径、认不出的形态)原样。
//!
//! 「之下」按**路径边界**判(`D:\proj` 不是 `D:\proj2\x` 的根),纯字符串运算,不碰
//! 磁盘、不解析符号链接。比较口径按**根的形态**定,不按编译平台:
//!
//! - Windows 形态(盘符 `X:\`、UNC `\\`):`\` 与 `/` 等价、大小写不敏感、根末尾带不带
//!   分隔符一样;目标带 `\\?\` verbatim 前缀也认;
//! - POSIX 形态(`/` 开头):只认 `/`、大小写敏感;根 `/` 本身不收(会吞掉一切);
//! - WSL UNC(`\\wsl$\<distro>\…` / `\\wsl.localhost\<distro>\…`,经
//!   [`mt_core::parse_wsl_unc`] 识别):发行版里是 Linux 文件系统,按发行版名(不分
//!   大小写)+ Linux 路径(区分大小写)判;两种 host 写法互认,在发行版里跑的 agent
//!   记下的 Linux 形态路径(`/home/u/proj/src/x`)也认。

use std::borrow::Cow;
use std::path::Path;

use mt_core::parse_wsl_unc;
use mt_core::path_key::{strip_verbatim_prefix, trim_trailing_separators};

/// Windows 形态的分隔符(两种等价)。
const WINDOWS_SEPS: &[char] = &['\\', '/'];
/// POSIX / Linux 形态的分隔符(`\` 是合法文件名字符)。
const POSIX_SEPS: &[char] = &['/'];

/// 路径类目标的缩短口径:项目根 + 用户家目录。`Default` = 都没有,原样不动。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPathScope {
    project: Option<Root>,
    home: Option<Root>,
}

impl ToolPathScope {
    /// 以 `project_root` 为项目根、当前用户家目录(`dirs::home_dir`)为 `~`。
    pub fn new(project_root: &str) -> Self {
        let home = dirs::home_dir();
        Self::with_home(project_root, home.as_deref().and_then(Path::to_str))
    }

    /// 显式给家目录(测试用;取不到家目录时传 None)。
    /// 认不出的根(空串、相对路径、单个 `/`)视同没有。
    pub fn with_home(project_root: &str, home: Option<&str>) -> Self {
        Self {
            project: Root::parse(project_root),
            home: home.and_then(Root::parse),
        }
    }

    /// 缩短一条路径(只缩短、不截断——截断归 `summarize_tool_target`,放在后面,
    /// 截完仍保住相对路径的开头)。口径见模块注释。
    pub fn shorten<'a>(&self, path: &'a str) -> Cow<'a, str> {
        if let Some(rest) = self.project.as_ref().and_then(|root| root.rest_of(path)) {
            return Cow::Borrowed(if rest.rel.is_empty() { "." } else { rest.rel });
        }
        if let Some(rest) = self.home.as_ref().and_then(|root| root.rest_of(path)) {
            return Cow::Owned(match rest.sep {
                Some(sep) => format!("~{sep}{}", rest.rel),
                None => "~".to_string(),
            });
        }
        Cow::Borrowed(path)
    }
}

/// 一个缩短用的根,存的是去掉尾随分隔符的形态。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Root {
    /// 盘符 / 非 WSL 的 UNC
    Windows(String),
    /// `/` 开头,非空
    Posix(String),
    /// WSL 发行版内:发行版名 + Linux 路径(发行版根为空串)
    Wsl { distro: String, unix: String },
}

/// 路径在根之下时,根后面剩下的部分。
struct Rest<'a> {
    /// 紧跟根的那个分隔符(原样,拼 `~` 时沿用);就是根本身时为 None
    sep: Option<char>,
    /// 去掉根与紧跟的分隔符后的相对段;就是根本身时为空串
    rel: &'a str,
}

impl<'a> Rest<'a> {
    /// `tail` 是根之后的原串(空串或以分隔符开头)。
    fn from_tail(tail: &'a str, seps: &[char]) -> Self {
        let rel = tail.trim_start_matches(seps);
        Self {
            sep: tail.chars().next().filter(|_| !rel.is_empty()),
            rel,
        }
    }
}

impl Root {
    fn parse(raw: &str) -> Option<Self> {
        let raw = strip_verbatim_prefix(raw.trim());
        if is_windows_absolute(&raw) {
            // `/` 换成 `\` 不改字节长度;parse_wsl_unc 只认 `\` 分隔
            if let Some(wsl) = parse_wsl_unc(&raw.replace('/', "\\")) {
                return Some(Self::Wsl {
                    distro: wsl.distro,
                    unix: wsl.unix_path.trim_end_matches('/').to_string(),
                });
            }
            let trimmed = trim_trailing_separators(&raw);
            // `\\` 之类只剩分隔符的根会吞掉一切,不收
            return (!trimmed.is_empty()).then(|| Self::Windows(trimmed.to_string()));
        }
        if raw.starts_with('/') {
            let trimmed = raw.trim_end_matches('/');
            return (!trimmed.is_empty()).then(|| Self::Posix(trimmed.to_string()));
        }
        None
    }

    fn rest_of<'a>(&self, path: &'a str) -> Option<Rest<'a>> {
        match self {
            Self::Windows(root) => {
                // verbatim 前缀只替换开头,根之后的部分仍是原串的后缀:
                // 在剥过的串上匹配,再按长度切回原串
                let stripped = strip_verbatim_prefix(path);
                let tail = strip_windows_root(&stripped, root)?;
                let tail = &path[path.len() - tail.len()..];
                Some(Rest::from_tail(tail, WINDOWS_SEPS))
            }
            Self::Posix(root) => {
                let tail = strip_posix_root(path, root)?;
                Some(Rest::from_tail(tail, POSIX_SEPS))
            }
            Self::Wsl { distro, unix } => {
                if !starts_with_two_seps(path) {
                    // 发行版里跑的 agent 记下的 Linux 形态路径
                    let tail = strip_posix_root(path, unix)?;
                    return Some(Rest::from_tail(tail, POSIX_SEPS));
                }
                let wsl = parse_wsl_unc(&path.replace('/', "\\"))?;
                // 发行版名只许 ASCII 字母数字与 `.` `-` `_`
                if !wsl.distro.eq_ignore_ascii_case(distro) {
                    return None;
                }
                let tail = strip_posix_root(&wsl.unix_path, unix)?;
                // unix_path = "/" + (原串后缀,`\` 换成 `/`,字节长度不变):有相对段时
                // tail 就对应原串同长的后缀;没有时(就是根本身)开头那个 `/` 可能是
                // parse_wsl_unc 补的,不能按长度切回
                if tail.trim_start_matches('/').is_empty() {
                    return Some(Rest { sep: None, rel: "" });
                }
                let tail = &path[path.len() - tail.len()..];
                Some(Rest::from_tail(tail, WINDOWS_SEPS))
            }
        }
    }
}

/// 盘符绝对路径(`X:\` / `X:/`)或 UNC(开头两个分隔符)。
fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    drive || starts_with_two_seps(path)
}

fn starts_with_two_seps(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && matches!(bytes[0], b'\\' | b'/') && matches!(bytes[1], b'\\' | b'/')
}

/// `path` 在 Windows 形态的 `root`(已去尾随分隔符)之下或就是它时,返回根之后的
/// 部分(空串或以分隔符开头)。逐字符比:`\` `/` 等价、大小写不敏感(Unicode 小写,
/// 与 `windows_eq_key` 同口径);不先整串转小写——个别字符小写后字节长度会变,
/// 就没法按位置切回原串了。
fn strip_windows_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let mut chars = path.chars();
    for r in root.chars() {
        let p = chars.next()?;
        let same = r == p
            || (WINDOWS_SEPS.contains(&r) && WINDOWS_SEPS.contains(&p))
            || r.to_lowercase().eq(p.to_lowercase());
        if !same {
            return None;
        }
    }
    let tail = chars.as_str();
    (tail.is_empty() || tail.starts_with(WINDOWS_SEPS)).then_some(tail)
}

/// `path` 在 POSIX 形态的 `root`(已去尾随 `/`;空串 = 发行版根)之下或就是它时,
/// 返回根之后的部分(空串或以 `/` 开头)。区分大小写。
fn strip_posix_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let tail = path.strip_prefix(root)?;
    let at_boundary = tail.starts_with('/') || (tail.is_empty() && !root.is_empty());
    at_boundary.then_some(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(root: &str, home: Option<&str>) -> ToolPathScope {
        ToolPathScope::with_home(root, home)
    }

    fn short(scope: &ToolPathScope, path: &str) -> String {
        scope.shorten(path).into_owned()
    }

    #[test]
    fn under_project_root_becomes_relative_keeping_separator_style() {
        let s = scope(r"D:\proj", None);
        assert_eq!(short(&s, r"D:\proj\src\cart.js"), r"src\cart.js");
        assert_eq!(short(&s, "D:/proj/src/cart.js"), "src/cart.js");
        // 混用:根之后原样
        assert_eq!(short(&s, r"D:/proj\src/cart.js"), "src/cart.js");
        assert_eq!(short(&s, r"D:\proj/src\cart.js"), r"src\cart.js");
        // 根与目标的分隔符风格不同
        let s = scope("D:/proj", None);
        assert_eq!(short(&s, r"D:\proj\src\cart.js"), r"src\cart.js");
    }

    #[test]
    fn windows_root_is_case_insensitive_and_keeps_target_case() {
        let s = scope(r"D:\Git\Proj", None);
        assert_eq!(short(&s, r"d:\git\PROJ\Src\Main.rs"), r"Src\Main.rs");
        // 非 ASCII 同样不分大小写
        let s = scope(r"D:\Ärger", None);
        assert_eq!(short(&s, r"d:\ärger\x.rs"), "x.rs");
    }

    #[test]
    fn root_trailing_separator_and_root_itself() {
        for root in [r"D:\proj", r"D:\proj\", "D:/proj/", r"D:\proj\/"] {
            let s = scope(root, None);
            assert_eq!(short(&s, r"D:\proj\a.rs"), "a.rs", "{root}");
            assert_eq!(short(&s, r"D:\proj"), ".", "{root}");
            assert_eq!(short(&s, r"D:\proj\"), ".", "{root}");
            assert_eq!(short(&s, "d:/PROJ"), ".", "{root}");
        }
        // 盘符根
        let s = scope(r"D:\", None);
        assert_eq!(short(&s, r"D:\proj\a.rs"), r"proj\a.rs");
    }

    #[test]
    fn sibling_with_same_prefix_is_not_under_root() {
        let s = scope(r"D:\proj", None);
        for path in [
            r"D:\proj2\x.rs",
            r"D:\project.rs",
            r"D:\proj-old\src\a.rs",
            r"E:\proj\a.rs",
            // 盘符相对路径不是绝对路径
            r"D:proj\a.rs",
        ] {
            assert_eq!(short(&s, path), path);
        }
    }

    #[test]
    fn home_prefix_becomes_tilde() {
        let s = scope(r"D:\proj", Some(r"C:\Users\Dev"));
        assert_eq!(
            short(&s, r"C:\Users\Dev\AppData\Local\Temp\x.txt"),
            r"~\AppData\Local\Temp\x.txt"
        );
        assert_eq!(short(&s, "c:/users/dev/.claude/a.md"), "~/.claude/a.md");
        assert_eq!(short(&s, r"C:\Users\Dev"), "~");
        assert_eq!(short(&s, r"C:\Users\Dev\"), "~");
        // 边界:同前缀的别的用户目录不算
        assert_eq!(short(&s, r"C:\Users\Dev2\x"), r"C:\Users\Dev2\x");
        // 都不沾:原样
        assert_eq!(short(&s, r"E:\data\x.csv"), r"E:\data\x.csv");
    }

    #[test]
    fn project_root_wins_over_home_when_nested() {
        let s = scope(r"C:\Users\Dev\proj", Some(r"C:\Users\Dev"));
        assert_eq!(short(&s, r"C:\Users\Dev\proj\src\a.rs"), r"src\a.rs");
        assert_eq!(short(&s, r"C:\Users\Dev\other\b.rs"), r"~\other\b.rs");
    }

    #[test]
    fn relative_and_unrecognized_paths_are_untouched() {
        let s = scope(r"D:\proj", Some(r"C:\Users\Dev"));
        for path in [
            "src/lib.rs",
            r"src\cart.js",
            ".",
            "..\\x",
            "a.txt",
            "",
            "**/*.rs",
        ] {
            assert_eq!(short(&s, path), path, "{path:?}");
        }
    }

    #[test]
    fn verbatim_target_prefix_is_recognized() {
        let s = scope(r"D:\proj", None);
        assert_eq!(short(&s, r"\\?\D:\proj\src\a.rs"), r"src\a.rs");
        // verbatim 的根同样认
        let s = scope(r"\\?\D:\proj", None);
        assert_eq!(short(&s, r"D:\proj\src\a.rs"), r"src\a.rs");
    }

    #[test]
    fn unrecognized_roots_disable_project_shortening() {
        for root in ["", "   ", "proj", r"proj\sub", "/", r"\\", "//"] {
            let s = scope(root, None);
            assert_eq!(s, ToolPathScope::default(), "{root:?}");
            assert_eq!(short(&s, r"D:\proj\a.rs"), r"D:\proj\a.rs");
        }
        // 家目录取不到:只做项目根
        let s = scope(r"D:\proj", None);
        assert_eq!(short(&s, r"C:\Users\Dev\x"), r"C:\Users\Dev\x");
    }

    #[test]
    fn posix_root_is_case_sensitive_with_boundary() {
        let s = scope("/home/u/proj/", Some("/home/u"));
        assert_eq!(short(&s, "/home/u/proj/src/main.rs"), "src/main.rs");
        assert_eq!(short(&s, "/home/u/proj"), ".");
        assert_eq!(short(&s, "/home/u/proj2/x"), "~/proj2/x");
        assert_eq!(short(&s, "/home/u/Proj/x"), "~/Proj/x");
        assert_eq!(short(&s, "/home/u"), "~");
        assert_eq!(short(&s, "/home/U/x"), "/home/U/x");
        assert_eq!(short(&s, "/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn wsl_unc_root_matches_both_hosts_and_linux_form() {
        for root in [
            r"\\wsl$\Ubuntu\home\u\proj",
            r"\\wsl.localhost\Ubuntu\home\u\proj\",
            r"\\?\UNC\wsl$\Ubuntu\home\u\proj",
            "//wsl$/Ubuntu/home/u/proj",
        ] {
            let s = scope(root, Some(r"C:\Users\Dev"));
            // 两种 host 写法互认,发行版名不分大小写,分隔符风格原样
            assert_eq!(
                short(&s, r"\\wsl$\Ubuntu\home\u\proj\src\a.rs"),
                r"src\a.rs",
                "{root}"
            );
            assert_eq!(
                short(&s, r"\\wsl.localhost\ubuntu\home\u\proj\src\a.rs"),
                r"src\a.rs",
                "{root}"
            );
            assert_eq!(
                short(&s, "//wsl.localhost/Ubuntu/home/u/proj/src/a.rs"),
                "src/a.rs",
                "{root}"
            );
            assert_eq!(
                short(&s, r"\\?\UNC\wsl$\Ubuntu\home\u\proj\x"),
                "x",
                "{root}"
            );
            assert_eq!(short(&s, r"\\wsl$\Ubuntu\home\u\proj"), ".", "{root}");
            assert_eq!(short(&s, r"\\wsl$\Ubuntu\home\u\proj\"), ".", "{root}");
            // 发行版里跑的 agent 记下的 Linux 形态
            assert_eq!(short(&s, "/home/u/proj/src/a.rs"), "src/a.rs", "{root}");
            assert_eq!(short(&s, "/home/u/proj"), ".", "{root}");
            // Linux 文件系统区分大小写;边界;别的发行版
            for other in [
                r"\\wsl$\Ubuntu\home\u\Proj\a.rs",
                r"\\wsl$\Ubuntu\home\u\proj2\a.rs",
                r"\\wsl$\Debian\home\u\proj\a.rs",
                "/home/u/Proj/a.rs",
                "/home/u/proj2/a.rs",
                r"\\server\share\home\u\proj\a.rs",
            ] {
                assert_eq!(short(&s, other), other, "{root} / {other}");
            }
            // 家目录照常
            assert_eq!(short(&s, r"C:\Users\Dev\x"), r"~\x", "{root}");
        }
    }

    #[test]
    fn wsl_distro_root() {
        let s = scope(r"\\wsl$\Ubuntu", None);
        assert_eq!(short(&s, r"\\wsl$\Ubuntu\etc\hosts"), r"etc\hosts");
        assert_eq!(short(&s, r"\\wsl$\Ubuntu"), ".");
        assert_eq!(short(&s, "/etc/hosts"), "etc/hosts");
        assert_eq!(short(&s, "etc/hosts"), "etc/hosts");
        assert_eq!(short(&s, ""), "");
    }

    #[test]
    fn plain_unc_root() {
        let s = scope(r"\\server\share\proj", None);
        assert_eq!(short(&s, r"\\SERVER\share\proj\a.rs"), "a.rs");
        assert_eq!(
            short(&s, r"\\server\share\proj2\a.rs"),
            r"\\server\share\proj2\a.rs"
        );
    }
}

//! 路径字符串的**比较键**与**形态变换**。纯字符串运算,不碰磁盘、不解析符号链接
//! (要物理身份请 [`crate::path::canonicalize`],用例见 `mt-project::watch`)。
//!
//! 同一条路径在程序里有好几种写法:用户填的 `D:\Git\x`、libgit2 给的 `D:/Git/x/`、
//! `canonicalize` 给的 `\\?\D:\Git\x`、notify 回来的大小写变体……比较前都得先归一,
//! 但**归一到什么程度取决于语义** —— 远程 POSIX 路径里的 `\` 是合法文件名字符,
//! 展示用的路径不能被转成小写。所以这里按语义各给一个函数,不造带开关的大一统函数:
//!
//! | 函数 | 分隔符 | 大小写 | 尾随分隔符 | 重复分隔符 | 用在哪 |
//! |---|---|---|---|---|---|
//! | [`windows_eq_key`] | `\` → `/` | 折叠 | 全去 | 保留 | Windows 语义的相等判断(项目去重、会话 cwd 匹配) |
//! | [`posix_ci_eq_key`] | 原样 | 折叠 | 去 `/` | 保留 | WSL drvfs / SSH 远程的 cwd 匹配 |
//! | [`slash_form`] | `\` → `/` | 原样 | 全去 | 保留 | 展示、拼 `前缀/` 算相对段 |
//! | [`collapse_separators`] | 连续 `\` `/` → 单个 `/` | 原样 | 全去(根留 `/`) | 折叠 | 前端 `replace(/[\\/]+/g, '/')` 同款 |
//! | [`trim_trailing_separators`] | 原样 | 原样 | 去 `/` 与 `\` | 原样 | 只差尾巴的比较 |
//! | [`posix_trim_trailing`] | 原样(`\` 不算分隔符) | 原样 | 去 `/`(根留 `/`) | 原样 | 远程 POSIX 目录比较 |
//! | [`strip_verbatim_prefix`] | — | — | — | — | 剥 `canonicalize` 的 `\\?\` / `\\?\UNC\` |
//!
//! 两个比较键**都不剥** `\\?\` 前缀 —— 需要时先 [`strip_verbatim_prefix`] 再取键。
//! 键只拿来比,不许落盘、不许展示(形态是实现细节,例如 [`windows_eq_key`] 产出
//! 正斜杠,调用方判「在某目录下」时要拼 `/`)。

use std::borrow::Cow;

/// Windows 语义的相等比较键:`\` 与 `/` 等价、不区分大小写、尾随分隔符不算差异。
///
/// 产出正斜杠 + 小写(Unicode 小写,与 NTFS 的大小写不敏感更接近)。**不折叠**重复
/// 分隔符(`\\server\share` 的开头两个 `\` 是 UNC 语义,不能折成一个)。
/// 平台无关:Linux 上对 POSIX 路径同样折叠大小写 —— 调用方(项目去重、会话 cwd
/// 匹配)一直是这个口径。
pub fn windows_eq_key(path: &str) -> String {
    let unified: String = path
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    unified.trim_end_matches('/').to_lowercase()
}

/// POSIX 路径的不区分大小写相等比较键:分隔符原样、去尾随 `/`、小写。
///
/// 用于 WSL 发行版内 / SSH 远程的 cwd 比较:`/mnt/*`(drvfs)默认大小写不敏感,
/// 同一目录可能以不同大小写出现。**不能**用 [`windows_eq_key`] 代替 —— 那个会把
/// POSIX 文件名里合法的 `\` 当成分隔符。
pub fn posix_ci_eq_key(path: &str) -> String {
    path.to_lowercase().trim_end_matches('/').to_string()
}

/// 展示 / 拼接用的正斜杠形态:`\` → `/`、去尾随分隔符,大小写原样。
///
/// libgit2 的 `workdir()` 一律给 `D:/Git/x/`,用户填的是 `D:\Git\x`,两边都转成
/// 这个形态后才能按字符串比较、拼 `{base}/` 前缀求相对段。
pub fn slash_form(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

/// 连续的 `\` / `/` 折成单个 `/`,再去掉尾随分隔符;整串只剩分隔符时留根 `/`。
/// 大小写原样。
///
/// 前端 `value.replace(/[\\/]+/g, '/')` 的同款。注意 UNC 开头的 `\\` 也会折成
/// 一个 `/` —— 只适合「两边都过同一个函数再比」的场合,产物不能再当路径用。
pub fn collapse_separators(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut last_sep = false;
    for ch in path.chars() {
        let is_sep = ch == '\\' || ch == '/';
        if !is_sep {
            out.push(ch);
        } else if !last_sep {
            out.push('/');
        }
        last_sep = is_sep;
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// 去掉尾随的 `/` 与 `\`(两种都算),其余原样。
pub fn trim_trailing_separators(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
}

/// POSIX 路径去尾随 `/`,只认 `/` 一种分隔符(远程文件名里的 `\` 是合法字符);
/// 整串全是 `/`(含空串)时返回根 `/`。
pub fn posix_trim_trailing(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/"
    } else {
        trimmed
    }
}

/// 剥掉 Windows verbatim 前缀(`Path::canonicalize` 在 Windows 上会加):
///
/// - `\\?\C:\foo` → `C:\foo`
/// - `\\?\UNC\server\share\x` → `\\server\share\x`(WSL 的 `\\?\UNC\wsl$\…` 同理)
/// - 其它 verbatim 形态(`\\?\Volume{GUID}\…` 等)与非 verbatim 路径原样返回
///
/// 纯字符串,跨平台行为一致。
pub fn strip_verbatim_prefix(path: &str) -> Cow<'_, str> {
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(path);
    };
    if let Some(unc_rest) = rest.strip_prefix(r"UNC\") {
        return Cow::Owned(format!(r"\\{unc_rest}"));
    }
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Cow::Borrowed(rest);
    }
    Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- windows_eq_key ----

    #[test]
    fn windows_key_folds_drive_letter_and_case() {
        assert_eq!(windows_eq_key(r"D:\Git\Repo"), "d:/git/repo");
        assert_eq!(
            windows_eq_key(r"d:\git\repo"),
            windows_eq_key(r"D:\GIT\Repo")
        );
        // 非 ASCII 也折叠(NTFS 大小写不敏感不止 ASCII)
        assert_eq!(windows_eq_key(r"D:\Ärger"), windows_eq_key(r"d:\ärger"));
    }

    #[test]
    fn windows_key_mixed_separators_are_equal() {
        assert_eq!(windows_eq_key(r"D:\Git/Repo\src"), "d:/git/repo/src");
        assert_eq!(
            windows_eq_key("D:/Git/Repo"),
            windows_eq_key(r"D:\Git\Repo")
        );
    }

    #[test]
    fn windows_key_ignores_trailing_separators() {
        assert_eq!(windows_eq_key(r"D:\Git\Repo\"), "d:/git/repo");
        assert_eq!(windows_eq_key("D:/Git/Repo//"), "d:/git/repo");
        assert_eq!(windows_eq_key(r"D:\Git\Repo\/"), "d:/git/repo");
        // 盘符根:尾随分隔符同样去掉
        assert_eq!(windows_eq_key(r"C:\"), "c:");
        // POSIX 形态(非 Windows 平台上的项目)照样适用
        assert_eq!(windows_eq_key("/home/U/Proj/"), "/home/u/proj");
    }

    #[test]
    fn windows_key_keeps_repeated_separators() {
        // 不折叠:与历史口径一致,也保住 UNC 开头的 `\\`
        assert_eq!(windows_eq_key(r"D:\\Git\Repo"), "d://git/repo");
        assert_ne!(
            windows_eq_key(r"D:\\Git\Repo"),
            windows_eq_key(r"D:\Git\Repo")
        );
    }

    #[test]
    fn windows_key_wsl_unc_forms() {
        assert_eq!(
            windows_eq_key(r"\\wsl.localhost\Ubuntu\home\u\"),
            "//wsl.localhost/ubuntu/home/u"
        );
        assert_eq!(
            windows_eq_key(r"\\WSL$\Ubuntu\home"),
            windows_eq_key("//wsl$/ubuntu/home")
        );
        // 两种 host 写法是两条不同的路径,键不合并
        assert_ne!(
            windows_eq_key(r"\\wsl$\Ubuntu\home"),
            windows_eq_key(r"\\wsl.localhost\Ubuntu\home")
        );
    }

    #[test]
    fn windows_key_does_not_strip_verbatim() {
        // 键本身不剥 `\\?\`;要忽略它就先 strip_verbatim_prefix 再取键
        assert_ne!(windows_eq_key(r"\\?\D:\Git\x"), windows_eq_key(r"D:\Git\x"));
        assert_eq!(
            windows_eq_key(&strip_verbatim_prefix(r"\\?\D:\Git\x")),
            windows_eq_key(r"d:/git/x/")
        );
        assert_eq!(
            windows_eq_key(&strip_verbatim_prefix(r"\\?\UNC\wsl$\Ubuntu\home")),
            windows_eq_key(r"\\wsl$\ubuntu\home")
        );
    }

    #[test]
    fn windows_key_empty_and_separator_only() {
        assert_eq!(windows_eq_key(""), "");
        assert_eq!(windows_eq_key("/"), "");
        assert_eq!(windows_eq_key(r"\"), "");
    }

    // ---- posix_ci_eq_key ----

    #[test]
    fn posix_key_lowercases_and_trims_trailing_slash() {
        assert_eq!(posix_ci_eq_key("/mnt/d/Git/Foo/"), "/mnt/d/git/foo");
        assert_eq!(posix_ci_eq_key("/home/User/proj"), "/home/user/proj");
        assert_eq!(posix_ci_eq_key("/home/u/proj//"), "/home/u/proj");
        assert_eq!(posix_ci_eq_key(""), "");
    }

    #[test]
    fn posix_key_leaves_backslash_alone() {
        // `\` 在 POSIX 文件名里是普通字符,不当分隔符(windows_eq_key 不可复用的原因)
        assert_eq!(posix_ci_eq_key(r"/home/u/a\b"), r"/home/u/a\b");
        assert_ne!(
            posix_ci_eq_key(r"/home/u/a\b"),
            posix_ci_eq_key("/home/u/a/b")
        );
        assert_eq!(posix_ci_eq_key(r"/home/u/x\"), r"/home/u/x\");
    }

    // ---- slash_form ----

    #[test]
    fn slash_form_unifies_separators_but_keeps_case() {
        assert_eq!(slash_form(r"D:\Git\Repo\"), "D:/Git/Repo");
        // libgit2 workdir 的形态与用户填的形态收敛到同一个串
        assert_eq!(slash_form("D:/Git/Repo/"), slash_form(r"D:\Git\Repo"));
        assert_ne!(slash_form(r"D:\Git\Repo"), slash_form(r"d:\git\repo"));
        assert_eq!(slash_form(r"D:\Git/Repo\\"), "D:/Git/Repo");
        assert_eq!(slash_form(r"\\wsl$\Ubuntu\home"), "//wsl$/Ubuntu/home");
        assert_eq!(slash_form(""), "");
        assert_eq!(slash_form("/"), "");
    }

    // ---- collapse_separators ----

    #[test]
    fn collapse_separators_unifies_and_trims() {
        assert_eq!(collapse_separators(r"D:\Git\demo\"), "D:/Git/demo");
        assert_eq!(collapse_separators("D:/Git/demo"), "D:/Git/demo");
        assert_eq!(collapse_separators(r"D:\\Git\\demo"), "D:/Git/demo");
        assert_eq!(collapse_separators(r"D:\Git/\demo//"), "D:/Git/demo");
        // 大小写原样
        assert_eq!(collapse_separators(r"D:\Git\Demo"), "D:/Git/Demo");
    }

    #[test]
    fn collapse_separators_root_and_empty() {
        assert_eq!(collapse_separators("/"), "/");
        assert_eq!(collapse_separators("//"), "/");
        assert_eq!(collapse_separators(r"\"), "/");
        assert_eq!(collapse_separators(""), "");
        assert_eq!(collapse_separators(r"C:\"), "C:");
    }

    #[test]
    fn collapse_separators_folds_unc_head() {
        // UNC 的 `\\` 也折成一个 `/`:只能用于「两边同过这个函数再比」
        assert_eq!(
            collapse_separators(r"\\wsl$\Ubuntu\home\"),
            "/wsl$/Ubuntu/home"
        );
        assert_eq!(
            collapse_separators(r"\\wsl.localhost\Ubuntu"),
            collapse_separators("//wsl.localhost/Ubuntu/")
        );
    }

    // ---- trim_trailing_separators ----

    #[test]
    fn trim_trailing_separators_handles_both_kinds() {
        assert_eq!(
            trim_trailing_separators(r"D:\Git\mini-term\"),
            r"D:\Git\mini-term"
        );
        assert_eq!(
            trim_trailing_separators(r"D:\Git\mini-term\/\"),
            r"D:\Git\mini-term"
        );
        assert_eq!(trim_trailing_separators("/home/u/proj//"), "/home/u/proj");
        // 中间的分隔符与大小写一概不动
        assert_eq!(trim_trailing_separators(r"D:\Git/X"), r"D:\Git/X");
        assert_eq!(trim_trailing_separators(""), "");
        assert_eq!(trim_trailing_separators("/"), "");
    }

    // ---- posix_trim_trailing ----

    #[test]
    fn posix_trim_trailing_keeps_root() {
        assert_eq!(posix_trim_trailing("/home/u/proj/"), "/home/u/proj");
        assert_eq!(posix_trim_trailing("/home/u/proj///"), "/home/u/proj");
        assert_eq!(posix_trim_trailing("/"), "/");
        assert_eq!(posix_trim_trailing("///"), "/");
        assert_eq!(posix_trim_trailing(""), "/");
        // `\` 不是 POSIX 分隔符
        assert_eq!(posix_trim_trailing(r"/home/u/x\"), r"/home/u/x\");
    }

    // ---- strip_verbatim_prefix ----

    #[test]
    fn strip_verbatim_drive_form() {
        assert_eq!(strip_verbatim_prefix(r"\\?\C:\foo\bar"), r"C:\foo\bar");
        assert_eq!(strip_verbatim_prefix(r"\\?\D:\"), r"D:\");
        assert!(matches!(
            strip_verbatim_prefix(r"\\?\C:\foo"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn strip_verbatim_unc_form() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\folder"),
            r"\\server\share\folder"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\wsl$\Ubuntu\home\user"),
            r"\\wsl$\Ubuntu\home\user"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\wsl.localhost\Ubuntu\home\user"),
            r"\\wsl.localhost\Ubuntu\home\user"
        );
        // host 大小写原样保留
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\Wsl.LocalHost\Ubuntu"),
            r"\\Wsl.LocalHost\Ubuntu"
        );
        // 只有 host 没有 share
        assert_eq!(strip_verbatim_prefix(r"\\?\UNC\wsl$"), r"\\wsl$");
    }

    #[test]
    fn strip_verbatim_leaves_other_forms_alone() {
        let guid = r"\\?\Volume{12345678-1234-1234-1234-123456789012}\foo";
        assert_eq!(strip_verbatim_prefix(guid), guid);
        for plain in [r"C:\foo", r"\\wsl$\Ubuntu\home", "/home/user", "", r"\\?\"] {
            assert_eq!(strip_verbatim_prefix(plain), plain, "{plain:?}");
        }
    }
}

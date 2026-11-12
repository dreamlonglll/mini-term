//! 快速打开查询的两步预处理:拆出末尾的 `:行[:列]`,再把文件那一段换成相对项目根的写法。
//!
//! [`split_goto`] 的拆法移植自 Winc(Apache-2.0)`crates/winc-app/src/app/workspace/picker.rs`
//! 的 `split_goto`:从右往左认冒号,冒号后面全是数字才拆。Windows 盘符路径里的冒号因此不会被
//! 当成行号 —— `C:\x\foo.rs:12` 从右边第一个冒号拆出 `12`,再往左那个冒号后面是 `\x\foo.rs`,
//! 不是数字,不拆。
//!
//! 与原文的差异:
//!
//! - 行列解析成数字,从 1 起:`0` 夹成 1,超出 `u32` 的夹到上限(文件页定位时会再按总行数夹);
//! - 末尾多出来的**一个** `:`(正在输入行号:`main.rs:`)先剥掉,结果列表不会因此闪成空的;
//! - 冒号两侧的空白不算数(`main.rs: 12`)。

use std::path::Path;

/// 查询末尾的跳转位置。行列都从 1 起。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Goto {
    pub line: u32,
    pub column: Option<u32>,
}

/// 把查询拆成「文件那一段」与末尾的 `:行[:列]`。
///
/// 例:`main.rs:42` → (`main.rs`, 42),`main.rs:42:7` → (`main.rs`, 42:7),`:42` → (空, 42),
/// `C:\x\foo.rs` → 原样、不拆。冒号后面不是数字时不拆。
pub fn split_goto(query: &str) -> (&str, Option<Goto>) {
    let query = query.trim();
    // 正在输入行号:末尾那个孤零零的 `:` 不参与匹配
    let query = match query.strip_suffix(':') {
        Some(head) if !head.is_empty() => head,
        _ => query,
    };
    let Some((head, last)) = query.rsplit_once(':') else {
        return (query, None);
    };
    let Some(last_number) = number(last) else {
        return (query, None);
    };
    match head.rsplit_once(':') {
        Some((file, line)) if number(line).is_some() => (
            file.trim_end(),
            Some(Goto {
                line: number(line).unwrap_or(1),
                column: Some(last_number),
            }),
        ),
        _ => (
            head.trim_end(),
            Some(Goto {
                line: last_number,
                column: None,
            }),
        ),
    }
}

/// 一段全是十进制数字的文本(两侧空白不算) → 数字,夹在 `1..=u32::MAX`。
fn number(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // 全是数字还解析失败只可能是溢出
    Some(text.parse::<u32>().unwrap_or(u32::MAX).max(1))
}

/// 文件那一段换成相对项目根、以 `/` 分隔的写法,供模糊匹配用。
///
/// - 反斜杠统一成 `/`(索引里存的就是 `/`);
/// - 是项目根下的绝对路径(从资源管理器 / 终端里复制来的)就去掉项目根前缀;Windows 上项目根
///   按 ASCII 大小写不敏感比较;
/// - 开头的 `./` 与 `/` 去掉:相对路径没有这两种前缀,留着必然匹配不上(与全局搜索
///   `search::normalize_path_query` 同口径)。
pub fn relative_query(root: &Path, file: &str) -> String {
    let unified = file.trim().replace('\\', "/");
    let root_text = root.to_string_lossy().replace('\\', "/");
    let root_text = root_text.trim_end_matches('/');
    if !root_text.is_empty()
        && let Some(rest) = strip_root(&unified, root_text)
    {
        return rest.trim_start_matches('/').to_string();
    }
    let rest = unified.strip_prefix("./").unwrap_or(&unified);
    rest.trim_start_matches('/').to_string()
}

/// `text` 以项目根 `root` 开头且紧跟一个 `/` 时返回 `/` 及其后的部分。
fn strip_root<'a>(text: &'a str, root: &str) -> Option<&'a str> {
    let head = text.get(..root.len())?;
    let same = if cfg!(windows) {
        head.eq_ignore_ascii_case(root)
    } else {
        head == root
    };
    let rest = &text[root.len()..];
    (same && rest.starts_with('/')).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goto(line: u32, column: Option<u32>) -> Option<Goto> {
        Some(Goto { line, column })
    }

    #[test]
    fn 拆出行号与列号() {
        assert_eq!(split_goto("foo.rs:12"), ("foo.rs", goto(12, None)));
        assert_eq!(split_goto("foo.rs:12:5"), ("foo.rs", goto(12, Some(5))));
        assert_eq!(split_goto("src/foo.rs:3"), ("src/foo.rs", goto(3, None)));
        assert_eq!(split_goto("foo.rs"), ("foo.rs", None));
        assert_eq!(split_goto(":42"), ("", goto(42, None)));
    }

    /// 冒号后面不是数字就不拆:文件名里本来就可能有冒号(Linux 上合法)。
    #[test]
    fn 冒号后面不是数字不拆() {
        assert_eq!(split_goto("a:b"), ("a:b", None));
        assert_eq!(split_goto("a:b:12"), ("a:b", goto(12, None)));
        assert_eq!(split_goto("foo.rs:x:5"), ("foo.rs:x", goto(5, None)));
    }

    /// Windows 盘符的冒号不当行号:只认最右边、后面是数字的那一两个冒号。
    #[test]
    fn 盘符路径的冒号不当行号() {
        assert_eq!(split_goto(r"C:\x\foo.rs"), (r"C:\x\foo.rs", None));
        assert_eq!(
            split_goto(r"C:\x\foo.rs:12"),
            (r"C:\x\foo.rs", goto(12, None))
        );
        assert_eq!(
            split_goto(r"C:\x\foo.rs:12:5"),
            (r"C:\x\foo.rs", goto(12, Some(5)))
        );
        assert_eq!(
            split_goto("D:/repo/a.rs:7"),
            ("D:/repo/a.rs", goto(7, None))
        );
        assert_eq!(
            split_goto(r"\\wsl$\Ubuntu\home\me\a.rs:9"),
            (r"\\wsl$\Ubuntu\home\me\a.rs", goto(9, None))
        );
    }

    /// 正在输入行号时末尾的 `:` 不参与匹配,列号输到一半同理。
    #[test]
    fn 末尾的冒号先剥掉() {
        assert_eq!(split_goto("foo.rs:"), ("foo.rs", None));
        assert_eq!(split_goto("foo.rs:12:"), ("foo.rs", goto(12, None)));
        assert_eq!(split_goto(r"C:\x\foo.rs:"), (r"C:\x\foo.rs", None));
        // 只剩一个冒号时原样留着(没有文件段可剥)
        assert_eq!(split_goto(":"), (":", None));
    }

    /// 行列从 1 起:`0` 夹成 1,溢出夹到上限;冒号两侧的空白不算数。
    #[test]
    fn 行列夹在合法范围() {
        assert_eq!(split_goto("foo.rs:0"), ("foo.rs", goto(1, None)));
        assert_eq!(split_goto("foo.rs:0:0"), ("foo.rs", goto(1, Some(1))));
        assert_eq!(
            split_goto("foo.rs:99999999999"),
            ("foo.rs", goto(u32::MAX, None))
        );
        assert_eq!(split_goto(" foo.rs : 12 "), ("foo.rs", goto(12, None)));
        // 纯数字的文件名不是行号
        assert_eq!(split_goto("404"), ("404", None));
    }

    #[test]
    fn 相对查询统一分隔符并去掉前缀() {
        let root = Path::new("/home/me/proj");
        assert_eq!(relative_query(root, "src\\main.rs"), "src/main.rs");
        assert_eq!(relative_query(root, "./src/main.rs"), "src/main.rs");
        assert_eq!(relative_query(root, "/src/main.rs"), "src/main.rs");
        assert_eq!(
            relative_query(root, "/home/me/proj/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(relative_query(root, "/home/me/proj/"), "");
        // 项目根只是字符串前缀、不在段边界上的不算
        assert_eq!(
            relative_query(root, "/home/me/project/a.rs"),
            "home/me/project/a.rs"
        );
        assert_eq!(relative_query(root, "main"), "main");
    }

    #[cfg(windows)]
    #[test]
    fn 项目根下的绝对路径去掉根前缀_盘符大小写不敏感() {
        let root = Path::new(r"D:\Git\mini-term");
        assert_eq!(
            relative_query(root, r"d:\git\Mini-Term\crates\mt-app\src\main.rs"),
            "crates/mt-app/src/main.rs"
        );
        assert_eq!(
            relative_query(
                Path::new(r"\\wsl$\Ubuntu\home\me\p"),
                r"\\wsl$\Ubuntu\home\me\p\a.rs"
            ),
            "a.rs"
        );
    }
}

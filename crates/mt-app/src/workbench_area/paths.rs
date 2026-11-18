//! 页签跟随文件树改动(重命名 / 移动 / 删除)要用的路径比较,纯函数、可测。
//!
//! 口径与文件页签去重([`super::normalize_local_document_path`])、文件树高亮
//! (`file_tree::tree_path_key`)一致:本地在 Windows 上不分大小写、`\` 与 `/` 等价;远程
//! (以及非 Windows 的本地)是 POSIX 原文 —— 大小写有意义,`\` 是文件名里的合法字符。
//! 一律**按整段**比:`foo` 不会配到 `foobar`,`a/b` 不会配到 `a/bc`。

use std::path::{Path, PathBuf};

/// 按哪种语义比路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathSemantics {
    /// Windows 本地:`\` 与 `/` 都是分隔符,不分大小写。
    Windows,
    /// POSIX:只有 `/` 是分隔符,大小写有意义。
    Posix,
}

impl PathSemantics {
    /// 文件页签的路径按哪种语义比:远程一律 POSIX,本地看宿主平台。
    pub(super) fn for_document(remote: bool) -> Self {
        if !remote && cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }

    fn is_separator(self, c: char) -> bool {
        c == '/' || (self == Self::Windows && c == '\\')
    }

    /// 非空的各段(重复分隔符、尾随分隔符不产生空段)。
    fn segments(self, path: &str) -> Vec<&str> {
        path.split(|c| self.is_separator(c))
            .filter(|segment| !segment.is_empty())
            .collect()
    }

    /// 开头有几个分隔符(最多算两个):`/a` 与 `a`、`\\server\share` 与 `\server\share`
    /// 段相同却不是同一个路径。
    fn anchor(self, path: &str) -> usize {
        path.chars()
            .take(2)
            .take_while(|c| self.is_separator(*c))
            .count()
    }

    fn same_segment(self, a: &str, b: &str) -> bool {
        match self {
            Self::Windows => a.to_lowercase() == b.to_lowercase(),
            Self::Posix => a == b,
        }
    }
}

/// `path` 就是 `base` 或在它之下时,返回 `base` 之后剩下的那几段(就是它本身时为空)。
fn segments_under<'a>(path: &'a str, base: &str, sem: PathSemantics) -> Option<Vec<&'a str>> {
    if sem.anchor(path) != sem.anchor(base) {
        return None;
    }
    let path_segments = sem.segments(path);
    let base_segments = sem.segments(base);
    if base_segments.is_empty() && sem.anchor(base) == 0 {
        // 空路径不是任何东西的祖先
        return None;
    }
    if path_segments.len() < base_segments.len() {
        return None;
    }
    let matches = base_segments
        .iter()
        .zip(&path_segments)
        .all(|(base, path)| sem.same_segment(base, path));
    matches.then(|| path_segments[base_segments.len()..].to_vec())
}

/// `path` 是不是 `base` 本身或在它之下(删除用:被删的是目录时,它下面的页签都算)。
pub(super) fn is_within(path: &Path, base: &Path, sem: PathSemantics) -> bool {
    segments_under(&path.to_string_lossy(), &base.to_string_lossy(), sem).is_some()
}

/// 重命名 / 移动之后 `path` 该换成什么:`path` 就是 `from` 或在它之下时,把 `from` 那一截
/// 换成 `to`、剩下的各段原样接上(分隔符随 `to`);不相干的路径返回 `None`。
pub(super) fn rebase(path: &Path, from: &Path, to: &Path, sem: PathSemantics) -> Option<PathBuf> {
    let path = path.to_string_lossy();
    let rest = segments_under(&path, &from.to_string_lossy(), sem)?;
    Some(PathBuf::from(join_segments(
        &to.to_string_lossy(),
        &rest,
        sem,
    )))
}

/// 路径的最后一段(页签标题):按同一套分隔符切,尾随分隔符不算。
pub(super) fn leaf_name(path: &Path, sem: PathSemantics) -> String {
    let text = path.to_string_lossy();
    sem.segments(&text)
        .last()
        .map(|segment| segment.to_string())
        .unwrap_or_else(|| text.into_owned())
}

/// 在 `base` 后面接上各段。分隔符跟 `base` 走:Windows 语义下 `base` 里只有 `/` 才用 `/`,
/// 否则用 `\`;POSIX 一律 `/`。
fn join_segments(base: &str, rest: &[&str], sem: PathSemantics) -> String {
    if rest.is_empty() {
        return base.to_string();
    }
    let separator = match sem {
        PathSemantics::Posix => '/',
        PathSemantics::Windows if base.contains('/') && !base.contains('\\') => '/',
        PathSemantics::Windows => '\\',
    };
    let mut out = base.trim_end_matches(|c| sem.is_separator(c)).to_string();
    for segment in rest {
        out.push(separator);
        out.push_str(segment);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebased(path: &str, from: &str, to: &str, sem: PathSemantics) -> Option<String> {
        rebase(Path::new(path), Path::new(from), Path::new(to), sem)
            .map(|path| path.to_string_lossy().into_owned())
    }

    /// 改名的就是这个文件 / 它所在的目录:换掉前缀、余下各段原样接上。
    #[test]
    fn 文件与所在目录改名后换前缀() {
        let posix = PathSemantics::Posix;
        assert_eq!(
            rebased("/p/src/a.rs", "/p/src/a.rs", "/p/src/b.rs", posix).as_deref(),
            Some("/p/src/b.rs")
        );
        assert_eq!(
            rebased("/p/src/deep/a.rs", "/p/src", "/p/lib", posix).as_deref(),
            Some("/p/lib/deep/a.rs")
        );
        // 移到别的目录下
        assert_eq!(
            rebased("/p/src/a.rs", "/p/src", "/p/old/src", posix).as_deref(),
            Some("/p/old/src/a.rs")
        );
        // 根是 `/` 也接得对
        assert_eq!(rebased("/a/b", "/a", "/", posix).as_deref(), Some("/b"));
    }

    /// 按整段比:`foo` 不配 `foobar`,`a/b` 不配 `a/bc`;不相干的路径不动。
    #[test]
    fn 同名前缀不算在其下() {
        for sem in [PathSemantics::Posix, PathSemantics::Windows] {
            assert_eq!(rebased("/p/foobar/a.rs", "/p/foo", "/p/x", sem), None);
            assert_eq!(rebased("/p/foobar", "/p/foo", "/p/x", sem), None);
            assert_eq!(rebased("/p/a/bc.rs", "/p/a/b", "/p/x", sem), None);
            assert_eq!(rebased("/q/a.rs", "/p", "/x", sem), None);
            assert!(!is_within(Path::new("/p/foobar"), Path::new("/p/foo"), sem));
            assert!(is_within(Path::new("/p/foo"), Path::new("/p/foo"), sem));
            assert!(is_within(Path::new("/p/foo/x"), Path::new("/p/foo/"), sem));
        }
    }

    /// Windows 本地:不分大小写、`\` 与 `/` 等价;余下各段保留原来的大小写,分隔符随新前缀。
    #[test]
    fn windows_语义不分大小写与分隔符() {
        let win = PathSemantics::Windows;
        assert_eq!(
            rebased(
                r"D:\Proj\SRC\Deep\A.rs",
                r"d:\proj\src",
                r"D:\proj\lib",
                win
            )
            .as_deref(),
            Some(r"D:\proj\lib\Deep\A.rs")
        );
        // 搜索拼出来的混合分隔符也认得
        assert_eq!(
            rebased(
                r"D:\proj\src/a.rs",
                r"D:\proj\src\a.rs",
                r"D:\proj\src\b.rs",
                win
            )
            .as_deref(),
            Some(r"D:\proj\src\b.rs")
        );
        assert!(is_within(
            Path::new(r"\\wsl$\Ubuntu\home\u\p\a.rs"),
            Path::new(r"\\WSL$\ubuntu\home\u\p"),
            win
        ));
        // UNC 的两个开头分隔符算数:`\server\share` 不是 `\\server\share`
        assert!(!is_within(
            Path::new(r"\server\share\a"),
            Path::new(r"\\server\share"),
            win
        ));
    }

    /// POSIX(远程):大小写有意义,`\` 是文件名的一部分,不是分隔符。
    #[test]
    fn posix_语义区分大小写且反斜杠不是分隔符() {
        let posix = PathSemantics::Posix;
        assert_eq!(rebased("/p/Src/a.rs", "/p/src", "/p/lib", posix), None);
        assert_eq!(rebased(r"/p/a\b.rs", "/p/a", "/p/x", posix), None);
        assert_eq!(
            rebased(r"/p/dir/a\b.rs", "/p/dir", "/p/new", posix).as_deref(),
            Some(r"/p/new/a\b.rs")
        );
        // 相对路径与绝对路径不混
        assert!(!is_within(Path::new("p/a"), Path::new("/p"), posix));
        // 空路径不是任何东西的祖先
        assert!(!is_within(Path::new("/p/a"), Path::new(""), posix));
    }

    #[test]
    fn 页签标题取最后一段() {
        assert_eq!(
            leaf_name(Path::new(r"D:\p\src\Main.rs"), PathSemantics::Windows),
            "Main.rs"
        );
        assert_eq!(
            leaf_name(Path::new(r"/p/a\b.rs"), PathSemantics::Posix),
            r"a\b.rs"
        );
        assert_eq!(leaf_name(Path::new("/p/dir/"), PathSemantics::Posix), "dir");
    }
}

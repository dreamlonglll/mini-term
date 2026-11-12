//! 本地路径的物理身份 —— 全仓唯一的 `canonicalize` 入口。
//!
//! `std::fs::canonicalize` 在 Windows 上给的是 verbatim 形态(`\\?\C:\x`、
//! `\\?\UNC\server\share\x`)。这种形态拿去与用户填的路径、libgit2 / notify 回报的
//! 路径比,一边带前缀一边不带,「在不在项目里」就判错;拖进 shell 也认不得。此前各处
//! 各自记得(或忘了)在 canonicalize 之后剥前缀,这里收成一个函数:调完 std 立刻剥
//! ([`strip_verbatim`]),全仓只剩一种形态。
//!
//! 根目录 `clippy.toml` 用 `disallowed-methods` 禁了 `std::fs::canonicalize` 与
//! `Path::canonicalize`,clippy 会把直接调用指到这里。真要 verbatim 形态的地方
//! (目前只有验证「verbatim 输入也能用」的测试)在调用处 `#[allow]` 并写明理由。
//!
//! 不剥的情形与 [`crate::path_key::strip_verbatim_prefix`] 一致:`\\?\Volume{GUID}\…`
//! 这类没有盘符 / UNC 等价写法的 verbatim 路径原样保留。

use std::io;
use std::path::{Path, PathBuf};

/// `std::fs::canonicalize`,再剥掉 Windows 的 verbatim 前缀(规则见 [`strip_verbatim`])。
///
/// 解析符号链接 / junction、`..` 与 8.3 短名,要求路径存在;错误原样返回。
#[allow(clippy::disallowed_methods)] // 全仓唯一允许直接调 std 的地方
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(strip_verbatim)
}

/// 剥掉 Windows verbatim 前缀:
///
/// - `\\?\C:\x` → `C:\x`
/// - `\\?\UNC\server\share\x` → `\\server\share\x`(WSL 的 `\\?\UNC\wsl.localhost\…`、
///   `\\?\UNC\wsl$\…` 同理,剥完就是能直接粘进 shell 的 `\\wsl.localhost\…`)
/// - 其它 verbatim 形态(`\\?\Volume{GUID}\…` 等)与普通路径原样返回
///
/// 非 Windows 平台原样返回:那边的 canonicalize 不加前缀,`\` 还是合法的文件名字符。
/// Windows 上路径不是合法 UTF-16 → UTF-8 时也原样返回 —— 宁可留着前缀,也不经 lossy
/// 转换把路径改坏(改坏的路径指向另一个文件)。
#[cfg(windows)]
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let stripped = path.to_str().and_then(|s| {
        let stripped = crate::path_key::strip_verbatim_prefix(s);
        // 没剥动 = 原串原样借回(长度不变);剥了一定变短
        (stripped.len() != s.len()).then(|| stripped.into_owned())
    });
    match stripped {
        Some(s) => PathBuf::from(s),
        None => path,
    }
}

/// 非 Windows:原样返回(见 Windows 版的说明)。
#[cfg(not(windows))]
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mt-core-path-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn canonicalize_不带_verbatim_前缀且结果稳定() {
        let dir = temp_dir("stable");
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();

        let canon = canonicalize(&file).unwrap();
        assert!(canon.is_absolute(), "{canon:?}");
        assert!(
            !canon.to_string_lossy().starts_with(r"\\?\"),
            "不应带 verbatim 前缀: {canon:?}"
        );
        // 已经规范过的路径再过一遍不变;经 `..` 绕一圈也落到同一个
        assert_eq!(canonicalize(&canon).unwrap(), canon);
        let roundabout = dir.join("..").join(dir.file_name().unwrap()).join("a.txt");
        assert_eq!(canonicalize(&roundabout).unwrap(), canon);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonicalize_不存在的路径报错() {
        let dir = temp_dir("missing");
        let err = canonicalize(dir.join("nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_剥盘符与_unc_两种形态() {
        let strip = |s: &str| strip_verbatim(PathBuf::from(s));
        assert_eq!(
            strip(r"\\?\C:\Users\u\proj"),
            PathBuf::from(r"C:\Users\u\proj")
        );
        assert_eq!(strip(r"\\?\D:\"), PathBuf::from(r"D:\"));
        assert_eq!(
            strip(r"\\?\UNC\server\share\x"),
            PathBuf::from(r"\\server\share\x")
        );
        assert_eq!(
            strip(r"\\?\UNC\wsl.localhost\Ubuntu\home\u"),
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\u")
        );
        assert_eq!(
            strip(r"\\?\UNC\wsl$\Ubuntu\home\u"),
            PathBuf::from(r"\\wsl$\Ubuntu\home\u")
        );
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_其它形态原样保留() {
        for s in [
            r"\\?\Volume{12345678-1234-1234-1234-123456789012}\foo",
            r"C:\Users\u",
            r"\\wsl.localhost\Ubuntu\home",
            "",
        ] {
            assert_eq!(strip_verbatim(PathBuf::from(s)), PathBuf::from(s));
        }
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_非_utf8_路径原样保留() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        // `\\?\C:\` + 落单的代理项:lossy 转换会把它换成 U+FFFD,指向另一个文件
        let mut wide: Vec<u16> = r"\\?\C:\".encode_utf16().collect();
        wide.push(0xD800);
        let path = PathBuf::from(OsString::from_wide(&wide));
        assert_eq!(strip_verbatim(path.clone()), path);
    }

    #[cfg(not(windows))]
    #[test]
    fn strip_verbatim_非_windows_原样返回() {
        // POSIX 文件名里 `\` 与 `?` 都合法,不能当前缀剥
        let p = PathBuf::from(r"\\?\C:\x");
        assert_eq!(strip_verbatim(p.clone()), p);
    }
}

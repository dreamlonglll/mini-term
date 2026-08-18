//! 文件树右键菜单要用的几件小事:路径拼接、在文件管理器里显示、后台跑文件操作。
//!
//! # 为什么路径拼接是手写字符串而不是 `Path::join`
//!
//! 这两个函数是**照抄** `src/components/FileTree.tsx` 的 `getRelativePath` 与
//! 那句 `` `${entry.path}${sep}${name}` ``:分隔符按**根路径里出现的是哪一种**
//! 来定(远程项目的路径是 POSIX 的,而本机是 Windows),`Path::join` 在 Windows
//! 上永远给 `\`,复制出来的相对路径就与装机版不一致了。
//!
//! # 「在文件夹中打开」为什么不在 mt-project 里
//!
//! `mt_project::editor` 只有 `open_path_with_default_app`(打开目录/按关联程序开
//! 文件),**没有** reveal 语义(打开父目录并选中该项)—— 原版走的是
//! `tauri-plugin-opener` 的 `revealItemInDir`。mt-project 本批只读,所以先在壳里
//! 落一份;缺口已记入交付说明。

use std::path::Path;
use std::process::Command;

/// 归一化路径分隔符:`[\\/]+` 折成单个 `/`,再去掉结尾那一个。
/// 逐条对应 TS 侧的 `value.replace(/[\\/]+/g, '/').replace(/\/$/, '')`。
fn normalize_sep(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_sep = false;
    for ch in value.chars() {
        let is_sep = ch == '/' || ch == '\\';
        if is_sep {
            if !prev_sep {
                out.push('/');
            }
        } else {
            out.push(ch);
        }
        prev_sep = is_sep;
    }
    if out.ends_with('/') {
        out.pop();
    }
    out
}

/// 该用哪种分隔符:根路径里出现过 `\` 就用 `\`,否则 `/`。
fn sep_of(root: &str) -> char {
    if root.contains('\\') { '\\' } else { '/' }
}

/// `target` 相对 `root` 的路径。
///
/// - 就是根本身 → `"."`;
/// - 不在根下面 → 原样返回(与原版一致:不猜、不报错);
/// - 否则 → 去掉根前缀,分隔符换回根用的那一种。
pub fn relative_path(target: &str, root: &str) -> String {
    let normalized_root = normalize_sep(root);
    let normalized_target = normalize_sep(target);
    let sep = sep_of(root);

    if normalized_target == normalized_root {
        return ".".to_string();
    }
    let prefix = format!("{normalized_root}/");
    if !normalized_target.starts_with(&prefix) {
        return target.to_string();
    }
    let rest = &normalized_target[prefix.len()..];
    if sep == '/' {
        rest.to_string()
    } else {
        rest.replace('/', "\\")
    }
}

/// 在目录 `dir` 下拼一个子项路径(新建文件/文件夹用)。
pub fn child_path(dir: &str, name: &str) -> String {
    // 原版判的是 `dir.includes('/')`(有 `/` 就用 `/`),与 `sep_of` 判 `\` 互为反面:
    // 纯 Windows 路径 `D:\a` 没有 `/` → `\`;POSIX 路径 `/home/u` → `/`。
    let sep = if dir.contains('/') { '/' } else { '\\' };
    format!("{dir}{sep}{name}")
}

/// 在系统文件管理器里显示该项(打开父目录并选中它)。
///
/// 阻塞(spawn 外部进程,网络盘/杀软下可能卡),调用方丢后台。
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // explorer 的 `/select,` 参数必须**整体不被二次转义**:走 raw_arg 自己带引号,
        // 否则 Rust 的 argv 转义会把带空格的路径包成 `"/select,C:\a b"`,explorer
        // 解析不出来会退化成「打开我的文档」。
        let mut cmd = Command::new("explorer.exe");
        cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        // explorer 成功时也常返回非 0 退出码,只 spawn 不等 status
        cmd.spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg("-R").arg(path).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Linux 没有统一的 reveal:退而求其次打开父目录(选中做不到)
        let target = path.parent().unwrap_or(path);
        Command::new("xdg-open").arg(target).spawn().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 相对路径按根的分隔符还原() {
        assert_eq!(
            relative_path("D:\\Git\\proj\\src\\main.rs", "D:\\Git\\proj"),
            "src\\main.rs"
        );
        // POSIX 根(远程项目)→ 用 `/`
        assert_eq!(relative_path("/home/u/proj/src/a.rs", "/home/u/proj"), "src/a.rs");
        // 混合分隔符也要认(watcher 回来的路径可能是另一种)
        assert_eq!(
            relative_path("D:/Git/proj/src/a.rs", "D:\\Git\\proj"),
            "src\\a.rs"
        );
    }

    #[test]
    fn 根自身是点不在根下原样返回() {
        assert_eq!(relative_path("D:\\Git\\proj", "D:\\Git\\proj"), ".");
        // 结尾多一个分隔符不影响判定
        assert_eq!(relative_path("D:\\Git\\proj\\", "D:\\Git\\proj"), ".");
        // 同前缀但不是子路径(proj2 不该被当成 proj 下的东西)
        assert_eq!(
            relative_path("D:\\Git\\proj2\\a.rs", "D:\\Git\\proj"),
            "D:\\Git\\proj2\\a.rs"
        );
    }

    #[test]
    fn 子项路径按目录已有的分隔符拼() {
        assert_eq!(child_path("D:\\Git\\proj", "a.rs"), "D:\\Git\\proj\\a.rs");
        assert_eq!(child_path("/home/u/proj", "a.rs"), "/home/u/proj/a.rs");
    }
}

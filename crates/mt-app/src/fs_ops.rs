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
//!
//! # 调外部程序一律走 [`open_external`]
//!
//! 编辑器 / 系统默认程序 / 浏览器 / 文件管理器,都是「后台 spawn,失败推 toast」
//! 这一套:spawn 外部进程在网络盘 / 杀软环境下会卡住调用线程,而失败(编辑器路径
//! 写错、文件已被删)只打日志的话,用户看到的就是「点了没反应」。
//! ⚠️ 弹窗里的入口别走它 —— toast 层在弹窗遮罩之下看不见,那种得自己丢后台、
//! 把错误就地显示(设置页「打开主题目录」就是这样)。

use std::path::{Path, PathBuf};
use std::process::Command;

use gpui::App;
use mt_core::path_key::collapse_separators;
use mt_project::editor::{Editor, EditorPosition};

use crate::i18n::t;

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
    // 分隔符归一逐条对应 TS 侧的 `value.replace(/[\\/]+/g, '/').replace(/\/$/, '')`
    // —— 那一句连根 `/` 也会去掉(折成空串),`collapse_separators` 留根,所以再
    // 剥一次尾 `/`:否则根是 `/` 时下面会拼出 `//` 前缀,一个都匹配不上
    let normalized_root = collapse_separators(root);
    let normalized_root = normalized_root.trim_end_matches('/');
    let normalized_target = collapse_separators(target);
    let normalized_target = normalized_target.trim_end_matches('/');
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

/// 按配置挑出「该用哪个外部编辑器」。`None` = 没配 → 调用方退到系统默认程序。
///
/// 纯函数,不碰 `App` —— 调用点普遍是「先 `store.read(cx)` 拿配置、再拿
/// `&mut App` 丢后台」,两次借用必须分开两句写才过得了借用检查。
pub fn configured_editor(config: &mt_config::AppConfig) -> Option<Editor> {
    let editors: Vec<Editor> = config
        .editors
        .iter()
        .map(|e| Editor {
            name: e.name.clone(),
            command: e.command.clone(),
        })
        .collect();
    mt_project::editor::select_editor(&editors, config.default_editor.as_deref(), None).cloned()
}

/// 「交给外部程序」的几种方式。
pub enum ExternalOpen {
    /// 外部编辑器;`None` = 没配 → 退到系统默认程序打开,别只给一句报错。
    Editor(Option<Editor>),
    /// 外部编辑器并跳到某一行(认得出跳行参数的编辑器才带上,见
    /// `mt_project::editor::editor_args`);`None` 同 [`Self::Editor`],退到系统默认程序、不带行。
    EditorAt(Option<Editor>, EditorPosition),
    /// 系统默认程序(文件按关联程序开,目录开文件管理器)。
    DefaultApp,
    /// 浏览器(走协议关联,见 `mt_project::editor::open_path_in_browser`)。
    Browser,
    /// 在文件管理器里显示(打开父目录并选中该项)。
    Reveal,
}

impl ExternalOpen {
    /// 阻塞执行。
    fn run(&self, path: &Path) -> anyhow::Result<()> {
        match self {
            Self::Editor(Some(editor)) => mt_project::editor::open_in_editor(Some(editor), path),
            Self::EditorAt(Some(editor), position) => {
                mt_project::editor::open_in_editor_at(Some(editor), path, Some(*position))
            }
            Self::Editor(None) | Self::EditorAt(None, _) | Self::DefaultApp => {
                mt_project::editor::open_path_with_default_app(path)
            }
            Self::Browser => mt_project::editor::open_path_in_browser(path),
            Self::Reveal => reveal_in_file_manager(path).map_err(Into::into),
        }
    }

    /// 失败 toast 的 (标题, 正文)。`detail` 是底层错误原文。
    ///
    /// 没配编辑器、退到默认程序又失败的那一档,顺带提示去设置里加编辑器 ——
    /// 用户要的本来就是「用编辑器打开」,光说「打开失败」没有下一步。
    fn failure_text(&self, detail: String) -> (String, String) {
        match self {
            Self::Editor(Some(_)) | Self::EditorAt(Some(_), _) => (
                t("fileTree", "dialog.openEditorFailedTitle").to_string(),
                detail,
            ),
            Self::Editor(None) | Self::EditorAt(None, _) => (
                t("fileTree", "dialog.noEditorTitle").to_string(),
                format!("{}\n{detail}", t("fileTree", "dialog.noEditorMessage")),
            ),
            Self::DefaultApp | Self::Browser | Self::Reveal => (
                t("fileTree", "dialog.openExternalFailedTitle").to_string(),
                detail,
            ),
        }
    }
}

/// 把 `path` 交给外部程序。**丢后台**跑,失败打日志并推一条 toast。
///
/// 只给非弹窗的入口用(理由见模块注释)。
pub fn open_external(how: ExternalOpen, path: PathBuf, cx: &mut App) {
    let task = cx.background_executor().spawn(async move {
        let result = how.run(&path);
        (how, path, result)
    });
    cx.spawn(async move |cx| {
        let (how, path, result) = task.await;
        let Err(err) = result else {
            return;
        };
        eprintln!("[files] 外部程序打开 {} 失败: {err:#}", path.display());
        let (title, message) = how.failure_text(format!("{err:#}"));
        cx.update(|cx| crate::toast::push_open_external_failure(title, message, cx));
    })
    .detach();
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
        // 重复分隔符折叠后再比
        assert_eq!(
            relative_path("D:\\\\Git\\proj\\\\src\\a.rs", "D:\\Git\\proj"),
            "src\\a.rs"
        );
    }

    /// 远程项目根就是 `/`:前缀不能拼成 `//`(与 TS 侧把根也折成空串同效)。
    #[test]
    fn 根为斜杠时照样算相对段() {
        assert_eq!(relative_path("/home/u/a.rs", "/"), "home/u/a.rs");
        assert_eq!(relative_path("/", "/"), ".");
        assert_eq!(relative_path("//", "/"), ".");
    }

    #[test]
    fn 子项路径按目录已有的分隔符拼() {
        assert_eq!(child_path("D:\\Git\\proj", "a.rs"), "D:\\Git\\proj\\a.rs");
        assert_eq!(child_path("/home/u/proj", "a.rs"), "/home/u/proj/a.rs");
    }
}

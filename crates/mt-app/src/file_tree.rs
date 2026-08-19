//! 中栏:文件树。对应 `src/components/FileTree.tsx` 的主干。
//!
//! - 列目录走 [`mt_project::fs::list_directory`](mt_project::fs::list_directory)
//!   (`.gitignore` 过滤与排序都在那边,这里不重复实现);它是**阻塞**函数,
//!   一律丢 background executor,不能在主线程上跑;
//! - 目录变化走 [`mt_project::watch::FsWatcher`](mt_project::watch::FsWatcher):
//!   sink 里往 channel 丢,主线程上的前台任务醒来后失效缓存并重列 ——
//!   与 AI 状态、终端重绘是同一套跨线程唤醒模式;
//! - 双击文件用 [`mt_project::editor`](mt_project::editor) 打开(编辑器取自配置)。
//!
//! 文件拖进终端(旧版把路径写进 PTY)本轮不做,gpui 的拖放另开一批。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task,
    Window, div, prelude::FluentBuilder, px,
};
use mt_project::fs::FileEntry;
use mt_project::watch::FsWatcher;
use mt_ui::icons::FileIcon;

use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, show_alert, show_prompt};
use crate::store::AppStore;
use crate::ui;

pub struct FileTree {
    store: Entity<AppStore>,
    /// 已列出的目录 → 子项。
    entries: HashMap<PathBuf, Vec<FileEntry>>,
    /// 正在列的目录(防重复排队)。
    loading: HashSet<PathBuf>,
    watcher: Arc<FsWatcher>,
    watched: HashSet<PathBuf>,
    /// 当前挂着的项目;换项目时整表作废。
    current_project: Option<String>,
    _fs_task: Task<()>,
}

impl FileTree {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this: &mut Self, _, cx| {
            this.sync_project(cx);
            cx.notify();
        })
        .detach();

        let (tx, mut rx) = mpsc::unbounded::<PathBuf>();
        let watcher = Arc::new(FsWatcher::new(move |change| {
            // notify 自己的线程:只把「哪个目录变了」丢过去,重列在主线程排。
            let dir = match change.path.parent() {
                Some(parent) => parent.to_path_buf(),
                None => change.path.clone(),
            };
            let _ = tx.unbounded_send(dir);
        }));

        let fs_task = cx.spawn(async move |this, cx| {
            while let Some(dir) = rx.next().await {
                if this
                    .update(cx, |tree: &mut FileTree, cx| {
                        tree.invalidate(&dir, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        let mut this = Self {
            store,
            entries: HashMap::new(),
            loading: HashSet::new(),
            watcher,
            watched: HashSet::new(),
            current_project: None,
            _fs_task: fs_task,
        };
        this.sync_project(cx);
        this
    }

    /// 活动项目变了:清空缓存与监听,重列根目录。
    fn sync_project(&mut self, cx: &mut Context<Self>) {
        let (project_id, root) = {
            let store = self.store.read(cx);
            match store.active_project() {
                Some(p) => (Some(p.id.clone()), Some(PathBuf::from(&p.path))),
                None => (None, None),
            }
        };
        if project_id == self.current_project {
            return;
        }
        for dir in std::mem::take(&mut self.watched) {
            self.watcher.unwatch(&dir);
        }
        self.entries.clear();
        self.loading.clear();
        self.current_project = project_id;
        if let Some(root) = root {
            self.load_dir(root.clone(), root, cx);
        }
    }

    fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .active_project()
            .map(|p| PathBuf::from(&p.path))
    }

    /// 列一个目录(后台线程)+ 挂监听。
    fn load_dir(&mut self, root: PathBuf, dir: PathBuf, cx: &mut Context<Self>) {
        if self.loading.contains(&dir) {
            return;
        }
        self.loading.insert(dir.clone());

        if self.watched.insert(dir.clone())
            && let Err(err) = self
                .watcher
                .watch(&dir, &root.to_string_lossy().to_string())
        {
            eprintln!("[files] 监听 {} 失败: {err:#}", dir.display());
        }

        let task_dir = dir.clone();
        let task_root = root.clone();
        cx.spawn(async move |this, cx| {
            // list_directory 是阻塞 IO(还要逐级读 .gitignore),必须离开主线程
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::fs::list_directory(&task_root, &task_dir) })
                .await;
            let _ = this.update(cx, |tree: &mut FileTree, cx| {
                tree.loading.remove(&dir);
                match result {
                    Ok(entries) => {
                        tree.entries.insert(dir, entries);
                    }
                    Err(err) => eprintln!("[files] 列目录失败: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 目录内容变了:已列过的重列一次。
    fn invalidate(&mut self, dir: &Path, cx: &mut Context<Self>) {
        if !self.entries.contains_key(dir) {
            return;
        }
        let Some(root) = self.project_root(cx) else {
            return;
        };
        self.load_dir(root, dir.to_path_buf(), cx);
    }

    fn toggle_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(project_id) = self.current_project.clone() else {
            return;
        };
        let key = path.to_string_lossy().to_string();
        let expanded = self.store.read(cx).is_dir_expanded(&project_id, &key);
        self.store.update(cx, |store, cx| {
            store.set_dir_expanded(&project_id, &key, !expanded, cx)
        });
        if !expanded {
            if let Some(root) = self.project_root(cx) {
                self.load_dir(root, path.clone(), cx);
            }
        } else {
            self.watched.remove(&path);
            self.watcher.unwatch(&path);
        }
        cx.notify();
    }

    fn open_file(&self, path: PathBuf, cx: &mut Context<Self>) {
        // 两句写:第一句借完配置就还,第二句才拿可变借用丢后台
        // (同一套挑编辑器 + 后台打开的逻辑,全局搜索点结果时也走它)
        let editor = crate::fs_ops::configured_editor(self.store.read(cx).config());
        crate::fs_ops::open_path_with(editor, path, cx);
    }

    /// 展开某个目录并(重)列它。新建文件/文件夹之后要用:原版是
    /// `if (!expanded) handleToggle(); else loadChildren();`。
    fn ensure_expanded(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(project_id) = self.current_project.clone() else {
            return;
        };
        // 项目根不是树里的一行,没有「展开」这回事 —— 真去置位的话
        // `expandedDirs` 里会多出一条根路径落进 config.json(装机版没有这一条)
        if self.project_root(cx).as_deref() != Some(dir.as_path()) {
            let key = dir.to_string_lossy().to_string();
            if !self.store.read(cx).is_dir_expanded(&project_id, &key) {
                self.store.update(cx, |store, cx| {
                    store.set_dir_expanded(&project_id, &key, true, cx)
                });
            }
        }
        self.reload_dir(dir, cx);
    }

    /// 重列一个目录(文件操作跑完之后)。
    ///
    /// 监听那一路(`FsWatcher`)也会来一次,这里多列一遍是为了**立刻**看到结果 ——
    /// notify 在 Windows 上有几十到几百毫秒的抖动窗口。`load_dir` 自带
    /// 「同一目录不重复排队」的闸门,两条路撞上也只列一次。
    fn reload_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if let Some(root) = self.project_root(cx) {
            self.load_dir(root, dir, cx);
        }
        cx.notify();
    }

    /// 把树按展开状态拍平成可渲染的行。
    fn rows(&self, project_id: &str, dir: &Path, depth: usize, cx: &App, out: &mut Vec<Row>) {
        let Some(entries) = self.entries.get(dir) else {
            return;
        };
        let store = self.store.read(cx);
        for entry in entries {
            let key = entry.path.to_string_lossy().to_string();
            let expanded = entry.is_dir && store.is_dir_expanded(project_id, &key);
            out.push(Row {
                name: entry.name.clone(),
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                ignored: entry.ignored,
                depth,
                expanded,
            });
            if expanded {
                self.rows(project_id, &entry.path, depth + 1, cx, out);
            }
        }
    }
}

#[derive(Clone)]
struct Row {
    name: String,
    path: PathBuf,
    is_dir: bool,
    ignored: bool,
    depth: usize,
    expanded: bool,
}

// ─── 右键菜单 ─────────────────────────────────────────────────

/// 文件树右键菜单的**项序**。`None` = 分隔线。
///
/// 逐条对照 `FileTree.tsx:210-325`。**跳过「查看变更」** —— git 那套 UI
/// (变更列表 / diff 面板)在 GPUI 侧还没有,菜单项点了没地方去。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileMenuAction {
    OpenWithDefault,
    CopyRelativePath,
    CopyAbsolutePath,
    RevealInFolder,
    Rename,
    Delete,
    NewFile,
    NewFolder,
}

fn file_menu_actions(is_dir: bool) -> Vec<Option<FileMenuAction>> {
    use FileMenuAction::*;
    let mut actions = Vec::new();
    // 原版是 `items.unshift(openWithDefault)`,所以只有文件才有、且排在最前
    if !is_dir {
        actions.push(Some(OpenWithDefault));
    }
    actions.extend([
        Some(CopyRelativePath),
        Some(CopyAbsolutePath),
        None,
        Some(RevealInFolder),
        None,
        Some(Rename),
        Some(Delete),
    ]);
    if is_dir {
        actions.extend([None, Some(NewFile), Some(NewFolder)]);
    }
    actions
}

/// 跑一件**阻塞**的文件操作:后台线程做事,回主线程刷目录 / 弹错误框。
///
/// `failure` 是 `fileTree.dialog.*` 里那对「标题 / 正文」key(正文带 `{error}`);
/// `None` = 只往 stderr 打一行不弹框 —— 新建文件/文件夹走的就是这一支:字典里
/// **没有** `createFailed*` 词条,而原版那两处 `invoke` 压根没接 catch(失败静默)。
/// 补词条要走 TS 源头 + 重新生成,不在本批里做,已记入交付说明。
fn spawn_fs_op(
    tree: Entity<FileTree>,
    refresh_dir: PathBuf,
    expand: bool,
    failure: Option<(&'static str, &'static str)>,
    op: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let task = cx.background_executor().spawn(async move { op() });
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok(()) => {
                    tree.update(cx, |tree, cx| {
                        if expand {
                            tree.ensure_expanded(refresh_dir, cx);
                        } else {
                            tree.reload_dir(refresh_dir, cx);
                        }
                    });
                }
                Err(err) => {
                    eprintln!("[files] 操作失败: {err:#}");
                    // 原版用 Tauri 的 message() 弹系统提示,这里统一走自己的 alert
                    if let Some((title, message)) = failure {
                        show_alert(
                            t("fileTree", title),
                            tr!("fileTree", message, error = format!("{err:#}")),
                            window,
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
}

/// 一行(文件/目录)的右键菜单。
fn file_menu(tree: &Entity<FileTree>, row: &Row, root: PathBuf) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for action in file_menu_actions(row.is_dir) {
        let Some(action) = action else {
            entries.push(menu::separator());
            continue;
        };
        let path = row.path.clone();
        let name = row.name.clone();
        let tree = tree.clone();
        let root = root.clone();
        // 父目录:重命名/删除之后要刷的是它;新建时刷的是目录自己
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone());

        entries.push(match action {
            FileMenuAction::OpenWithDefault => {
                menu::item(t("fileTree", "menu.openWithDefault"), move |_window, cx| {
                    let path = path.clone();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(err) = mt_project::editor::open_path_with_default_app(&path) {
                                eprintln!("[files] 默认程序打开失败: {err:#}");
                            }
                        })
                        .detach();
                })
            }
            FileMenuAction::CopyRelativePath => {
                let relative =
                    fs_ops::relative_path(&path.to_string_lossy(), &root.to_string_lossy());
                menu::item(t("fileTree", "menu.copyRelativePath"), move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()));
                })
            }
            FileMenuAction::CopyAbsolutePath => {
                let absolute = path.to_string_lossy().to_string();
                menu::item(t("fileTree", "menu.copyAbsolutePath"), move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(absolute.clone()));
                })
            }
            FileMenuAction::RevealInFolder => {
                menu::item(t("fileTree", "menu.revealInFolder"), move |_window, cx| {
                    let path = path.clone();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(err) = fs_ops::reveal_in_file_manager(&path) {
                                eprintln!("[files] 在文件夹中打开失败: {err}");
                            }
                        })
                        .detach();
                })
            }
            FileMenuAction::Rename => menu::item(t("fileTree", "menu.rename"), move |window, cx| {
                let (tree, root, path, parent) =
                    (tree.clone(), root.clone(), path.clone(), parent.clone());
                let old_name = name.clone();
                show_prompt(
                    t("fileTree", "prompt.renameTitle"),
                    t("fileTree", "prompt.renameMessage"),
                    old_name.clone(),
                    move |value, window, cx| {
                        let new_name = value.trim().to_string();
                        // 空名 / 没改都当没点(原版同一条判断)
                        if new_name.is_empty() || new_name == old_name {
                            return;
                        }
                        let (root, path) = (root.clone(), path.clone());
                        spawn_fs_op(
                            tree.clone(),
                            parent.clone(),
                            false,
                            Some(("dialog.renameFailedTitle", "dialog.renameFailedMessage")),
                            move || mt_project::fs::rename_entry(&root, &path, &new_name).map(|_| ()),
                            window,
                            cx,
                        );
                    },
                    window,
                    cx,
                );
            }),
            FileMenuAction::Delete => {
                let is_dir = row.is_dir;
                MenuItem::new(t("fileTree", "menu.delete"))
                    .danger()
                    .on_click(move |window, cx| {
                        let (tree, root, path, parent) =
                            (tree.clone(), root.clone(), path.clone(), parent.clone());
                        let (title, message) = if is_dir {
                            (
                                t("fileTree", "dialog.deleteFolderTitle"),
                                tr!("fileTree", "dialog.deleteConfirmFolder", name = name),
                            )
                        } else {
                            (
                                t("fileTree", "dialog.deleteFileTitle"),
                                tr!("fileTree", "dialog.deleteConfirmFile", name = name),
                            )
                        };
                        Confirm::new(title, message)
                            .ok_text(t("fileTree", "dialog.deleteOk"))
                            .cancel_text(t("fileTree", "dialog.deleteCancel"))
                            .open(
                                move |window, cx| {
                                    let (root, path) = (root.clone(), path.clone());
                                    spawn_fs_op(
                                        tree.clone(),
                                        parent.clone(),
                                        false,
                                        Some((
                                            "dialog.deleteFailedTitle",
                                            "dialog.deleteFailedMessage",
                                        )),
                                        move || mt_project::fs::delete_entry(&root, &path),
                                        window,
                                        cx,
                                    );
                                },
                                window,
                                cx,
                            );
                    })
                    .into()
            }
            FileMenuAction::NewFile => {
                menu::item(t("fileTree", "menu.newFile"), move |window, cx| {
                    new_entry_prompt(tree.clone(), root.clone(), path.clone(), false, window, cx);
                })
            }
            FileMenuAction::NewFolder => {
                menu::item(t("fileTree", "menu.newFolder"), move |window, cx| {
                    new_entry_prompt(tree.clone(), root.clone(), path.clone(), true, window, cx);
                })
            }
        });
    }
    entries
}

/// 「新建文件 / 新建文件夹」:问名字 → 建 → 展开父目录并重列。
fn new_entry_prompt(
    tree: Entity<FileTree>,
    root: PathBuf,
    dir: PathBuf,
    is_dir: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let (title, message) = if is_dir {
        (
            t("fileTree", "prompt.newFolderTitle"),
            t("fileTree", "prompt.newFolderMessage"),
        )
    } else {
        (
            t("fileTree", "prompt.newFileTitle"),
            t("fileTree", "prompt.newFileMessage"),
        )
    };
    show_prompt(
        title,
        message,
        "",
        move |value, window, cx| {
            let name = value.trim().to_string();
            if name.is_empty() {
                return;
            }
            // 分隔符按目录路径里已有的那种拼(远程/WSL 路径是 POSIX 的)
            let target = PathBuf::from(fs_ops::child_path(&dir.to_string_lossy(), &name));
            let root = root.clone();
            spawn_fs_op(
                tree.clone(),
                dir.clone(),
                // 建完把父目录展开,不然新建的东西看不见(原版 `if (!expanded) handleToggle()`)
                true,
                // 缺 `createFailed*` 词条,失败只打日志(见 spawn_fs_op 的说明)
                None,
                move || {
                    if is_dir {
                        mt_project::fs::create_directory(&root, &target)
                    } else {
                        mt_project::fs::create_file(&root, &target)
                    }
                },
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .items_center()
            .px(px(10.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .child(t("panels", "files"));

        let Some(project_id) = self.current_project.clone() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header);
        };
        let Some(root) = self.project_root(cx) else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header);
        };

        let mut rows = Vec::new();
        self.rows(&project_id, &root, 0, cx, &mut rows);

        let mut list = div()
            .id("file-tree-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            // 空白处右键 = 在项目根新建(原版 `handleRootContextMenu`)。
            // 行自己会 stop_propagation,所以点在行上不会走到这儿。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let Some(root) = this.project_root(cx) else {
                        return;
                    };
                    let tree = cx.entity();
                    let entries = vec![
                        {
                            let (tree, root) = (tree.clone(), root.clone());
                            menu::item(t("fileTree", "menu.newFile"), move |window, cx| {
                                new_entry_prompt(
                                    tree.clone(),
                                    root.clone(),
                                    root.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                        },
                        {
                            let (tree, root) = (tree.clone(), root.clone());
                            menu::item(t("fileTree", "menu.newFolder"), move |window, cx| {
                                new_entry_prompt(
                                    tree.clone(),
                                    root.clone(),
                                    root.clone(),
                                    true,
                                    window,
                                    cx,
                                );
                            })
                        },
                    ];
                    menu::show(event.position, entries, window, cx);
                }),
            );
        for row in rows {
            list = list.child(self.render_row(row, cx));
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(header)
            .child(list)
    }
}

impl FileTree {
    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let path = row.path.clone();
        let is_dir = row.is_dir;
        let row_for_menu = row.clone();
        let indent = px(6.0 + row.depth as f32 * 12.0);
        let color = if row.ignored {
            ui::text_muted()
        } else if row.is_dir {
            ui::color_folder()
        } else {
            ui::color_file()
        };
        // 图标按文件名/是否目录/是否展开取类别(`FileIcon` 内含 53 类映射,
        // 「特殊文件名压扩展名」的语义也在那边:Cargo.lock 是锁文件不是 toml)。
        //
        // `.gitignore` 掉的条目统一压成 muted,与文字同色;其余用类别自带的
        // 语言色。git 状态着色(修改/新增/冲突)是后续批次,这里先不传。
        let icon = {
            let icon = FileIcon::new(&row.name, row.is_dir, row.expanded).size(px(14.0));
            if row.ignored {
                icon.color(ui::text_muted())
            } else {
                icon
            }
        };

        div()
            .id(SharedString::from(format!("fs-{}", row.path.display())))
            .flex()
            .items_center()
            .gap(px(4.0))
            .pl(indent)
            .pr(px(6.0))
            .py(px(2.0))
            .cursor_pointer()
            .text_size(ui::font_px(12.0))
            .text_color(color)
            .hover(|el| el.bg(ui::bg_overlay()))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, cx| {
                if is_dir {
                    this.toggle_dir(path.clone(), cx);
                } else if event.click_count() >= 2 {
                    this.open_file(path.clone(), cx);
                }
            }))
            // 行的右键菜单。**必须 stop_propagation** —— 否则会连带触发列表容器
            // 那个「空白处右键 = 新建」的菜单(原版靠 `e.stopPropagation()` 同理)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(root) = this.project_root(cx) else {
                        return;
                    };
                    let entries = file_menu(&cx.entity(), &row_for_menu, root);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .child(
                div()
                    .w(px(10.0))
                    .text_color(ui::text_muted())
                    .when(row.is_dir, |el| {
                        el.child(if row.expanded { "▾" } else { "▸" })
                    }),
            )
            .child(icon)
            .child(div().child(row.name))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::FileMenuAction::*;
    use super::*;

    /// 文件的菜单:「使用默认工具打开」在最前(原版 unshift),没有「新建」两项。
    #[test]
    fn 文件菜单项序与原版一致() {
        assert_eq!(
            file_menu_actions(false),
            vec![
                Some(OpenWithDefault),
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                None,
                Some(RevealInFolder),
                None,
                Some(Rename),
                Some(Delete),
            ]
        );
    }

    /// 目录的菜单:没有「默认工具打开」,末尾多一段「新建文件 / 新建文件夹」。
    #[test]
    fn 目录菜单项序与原版一致() {
        assert_eq!(
            file_menu_actions(true),
            vec![
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                None,
                Some(RevealInFolder),
                None,
                Some(Rename),
                Some(Delete),
                None,
                Some(NewFile),
                Some(NewFolder),
            ]
        );
    }

    /// 「查看变更」在两种菜单里都不许出现(git UI 未建),
    /// 而「默认工具打开」只对文件出现。
    #[test]
    fn 目录与文件的差别只在两处() {
        let file: Vec<_> = file_menu_actions(false).into_iter().flatten().collect();
        let dir: Vec<_> = file_menu_actions(true).into_iter().flatten().collect();
        assert!(file.contains(&OpenWithDefault));
        assert!(!dir.contains(&OpenWithDefault));
        assert!(dir.contains(&NewFile) && dir.contains(&NewFolder));
        assert!(!file.contains(&NewFile) && !file.contains(&NewFolder));
    }
}

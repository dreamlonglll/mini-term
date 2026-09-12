//! 文件树的「移动」:落点判据 [`MoveSource`] + 右键「移动到 ▸」的多级目录面板
//! [`MoveToPanel`]。
//!
//! ```text
//! 行右键 ─→ 菜单项「移动到」悬停
//!            └─ menu.rs 的自绘子菜单挂载点(MenuItem::submenu_element)
//!                 └─ MoveToPanel(项目根)            ← 本模块
//!                      ├─ 「项目根目录」(= 移到这一层)
//!                      ├─ src ▸ ── 悬停 ──→ MoveToPanel(src)
//!                      │                     ├─ 「移动到此处」
//!                      │                     └─ core ▸ ──→ …
//!                      └─ docs ▸
//! ```
//!
//! # 为什么不是 `MenuItem::submenu` 嵌套
//!
//! 普通子菜单的条目在菜单打开那一刻就得**全部**给出,而项目的目录树只有展开过的
//! 那部分在文件树的缓存里;要多级就得递归列整棵树 —— 本地大仓几百次 `list_directory`
//! (每次还要跑 gitignore 匹配),远程更是一次一趟 SFTP 往返,右键弹菜单不能等它。
//! 于是照 [`crate::branch_family`] 的路子挂自绘面板:**每一层悬停到才列**,列过缓存
//! 在面板实体里,菜单收起时整棵面板树随闭包一起释放。
//!
//! # 子面板挂在哪
//!
//! 列表区可滚(目录多的那一层不能溢出窗口),而 gpui 的 `overflow_y_scroll` 会把
//! 两个轴一起裁 —— 子面板要是挂在行里(菜单基件那种 `absolute left:100%` 的挂法),
//! 展开就被裁没。所以子面板挂在**面板根**上、与滚动容器平级,纵向位置取
//! 行在上一帧记下的矩形(`row_bounds`,与 `terminal_area` 记 pane 矩形同一手法)。
//! 不能用 `deferred` 逃出裁剪:菜单层自己就是一个 deferred 绘制,gpui 禁止在
//! deferred 里再 defer(`prepaint_deferred_draws` 里那句 assert)。
//!
//! # 什么落点不接
//!
//! - 已在这个目录里(源的父目录);
//! - 源是目录时:自身、及自身的任何子孙(`fs::rename` 遇到这种情形各平台行为不一,
//!   后端也拦,这里是让菜单项置灰 / 拖拽不亮)。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window,
    anchored, canvas, div, prelude::FluentBuilder, px, relative,
};
use mt_config::SshConnection;
use mt_project::fs::FileEntry;
use mt_ui::icons::FileIcon;

use crate::file_ops::FileOperationContext;
use crate::i18n::t;
use crate::menu::{self, MenuEntry, MenuItem};
use crate::ui;

use super::FileTree;
use super::ops::start_move;

/// 列表区最大高度;超过就滚(与 `branch_family` 同一档)。
const MAX_LIST_HEIGHT: f32 = 320.0;

/// 被移动的那一项。拖拽载荷([`crate::dnd::DragFilePath`])与右键行都能凑出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MoveSource {
    pub path: PathBuf,
    /// 树里的显示名(压缩链行是 `src/main/java`),确认框用。
    pub name: String,
    pub is_dir: bool,
    /// 源所在目录。远程按 POSIX 切、本地按平台切 —— 调用方按后端算好传进来,
    /// 这里不再猜分隔符(远端文件名里的反斜杠在 Windows 客户端上会被 `Path` 当分隔符)。
    pub parent: PathBuf,
    pub remote: bool,
}

impl MoveSource {
    /// 这个目录能不能接住这次移动(拖拽高亮 / 落下、菜单项置灰共用的判据)。
    pub fn accepts_target(&self, target_dir: &Path) -> bool {
        if same_path(self.remote, &self.parent, target_dir) {
            return false;
        }
        if self.is_dir && is_same_or_descendant(self.remote, &self.path, target_dir) {
            return false;
        }
        true
    }
}

/// 两条路径指向同一目录吗。远程走字符串(去尾 `/`),本地走 `Path` 的按段比较
/// (尾分隔符、`.` 段都不算差异)。
fn same_path(remote: bool, a: &Path, b: &Path) -> bool {
    if remote {
        let a = a.to_string_lossy();
        let b = b.to_string_lossy();
        trim_posix(&a) == trim_posix(&b)
    } else {
        a == b
    }
}

/// `path` 是 `ancestor` 自身或它的子孙吗。
fn is_same_or_descendant(remote: bool, ancestor: &Path, path: &Path) -> bool {
    if remote {
        crate::remote_ssh::posix_relative(&ancestor.to_string_lossy(), &path.to_string_lossy())
            .is_some()
    } else {
        path.starts_with(ancestor)
    }
}

fn trim_posix(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// 「移动到 ▸」菜单项。
///
/// 根面板实体**懒建一次**并缓存在闭包里(理由见 [`crate::branch_family::view_branches_menu_item`]:
/// `submenu_element` 每次菜单重绘都会调,每次新建等于每帧重列一遍目录)。
pub(super) fn move_to_menu_item(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<SshConnection>,
    source: MoveSource,
) -> MenuEntry {
    let cached: RefCell<Option<Entity<MoveToPanel>>> = RefCell::new(None);
    MenuItem::new(t("fileTree", "menu.moveTo"))
        .submenu_element(move |_window, cx| {
            let mut slot = cached.borrow_mut();
            let panel = slot.get_or_insert_with(|| {
                let (tree, context, connection, source) = (
                    tree.clone(),
                    context.clone(),
                    connection.clone(),
                    source.clone(),
                );
                let root = context.root.clone();
                cx.new(|cx| MoveToPanel::new(tree, context, connection, source, root, cx))
            });
            panel.clone().into_any_element()
        })
        .into()
}

/// 一层目录面板。子面板是它的子实体,按子目录路径缓存。
pub(super) struct MoveToPanel {
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<SshConnection>,
    source: MoveSource,
    /// 本面板列的目录。
    dir: PathBuf,
    /// `None` = 还在列;`Some(Err)` = 列失败(那一层只剩「移动到此处」)。
    children: Option<Result<Vec<FileEntry>, String>>,
    /// 悬停展开的子目录下标。
    open_child: Option<usize>,
    child_panels: HashMap<PathBuf, Entity<MoveToPanel>>,
    /// 面板根与各子目录行上一帧的矩形(子面板定位用,见模块注释)。
    root_bounds: Option<Bounds<Pixels>>,
    row_bounds: HashMap<usize, Bounds<Pixels>>,
    /// 列目录的任务。面板随菜单收起而 drop,没回来的列表跟着取消 —— 菜单都关了,
    /// 结果没人看。
    _task: Option<Task<()>>,
}

impl MoveToPanel {
    fn new(
        tree: Entity<FileTree>,
        context: FileOperationContext,
        connection: Option<SshConnection>,
        source: MoveSource,
        dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let root = context.root.clone();
        let list_dir = dir.clone();
        let remote = connection.clone();
        // 两条路都是阻塞 IO(本地逐级读 .gitignore,远程 SFTP 往返),离开主线程
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::remote_ssh::list_directory_for(remote.as_ref(), &root, &list_dir, false)
                        .map(|entries| entries.into_iter().filter(|e| e.is_dir).collect())
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.children = Some(result);
                cx.notify();
            });
        });
        Self {
            tree,
            context,
            connection,
            source,
            dir,
            children: None,
            open_child: None,
            child_panels: HashMap::new(),
            root_bounds: None,
            row_bounds: HashMap::new(),
            _task: Some(task),
        }
    }

    fn is_root(&self) -> bool {
        same_path(self.source.remote, &self.dir, &self.context.root)
    }

    fn subdirs(&self) -> &[FileEntry] {
        match self.children.as_ref() {
            Some(Ok(dirs)) => dirs,
            _ => &[],
        }
    }

    /// 悬停到第 `index` 个子目录:展开它(没建过面板就建,建即开始列)。
    /// `None` = 悬停到别的行,收起子面板。
    fn set_open_child(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        if self.open_child == index {
            return;
        }
        if let Some(i) = index
            && let Some(entry) = self.subdirs().get(i).cloned()
            && !self.child_panels.contains_key(&entry.path)
        {
            let (tree, context, connection, source) = (
                self.tree.clone(),
                self.context.clone(),
                self.connection.clone(),
                self.source.clone(),
            );
            let dir = entry.path.clone();
            let panel = cx.new(|cx| Self::new(tree, context, connection, source, dir, cx));
            self.child_panels.insert(entry.path, panel);
        }
        self.open_child = index;
        cx.notify();
    }

    /// 点了一个目标:先收菜单再(延后一拍)动手。
    ///
    /// 顺序与 `branch_family` 那处同一条理由:收菜单会 drop 整棵面板(实体被菜单项
    /// 的闭包持有),在自己的 listener 里同步跑后续动作等于站在正在塌的楼上,
    /// 所以移动 defer 出去;而菜单关闭还会把焦点还给打开前那个元素,先收再动
    /// 才不会把刚弹出的忙碌/失败提示的焦点抢走。
    fn pick(&self, target_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.source.accepts_target(&target_dir) {
            return;
        }
        let (tree, context, connection, source) = (
            self.tree.clone(),
            self.context.clone(),
            self.connection.clone(),
            self.source.clone(),
        );
        window.defer(cx, move |window, cx| {
            start_move(tree, context, connection, source, target_dir, window, cx);
        });
        menu::close(window, cx);
    }

    fn hint(text: impl Into<SharedString>) -> AnyElement {
        div()
            .px(px(12.0))
            .py(px(6.0))
            .text_color(ui::text_muted())
            .child(text.into())
            .into_any_element()
    }

    /// 一行(「移动到此处」/ 某个子目录)的公共壳:与菜单项同尺寸同配色。
    fn row(id: SharedString, enabled: bool, highlighted: bool) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .relative()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .text_color(ui::text_secondary())
            .when(!enabled, |el| el.opacity(0.4))
            .when(enabled, |el| {
                el.cursor_pointer().hover(|el| {
                    el.bg(ui::with_alpha(ui::accent(), 0.2))
                        .text_color(ui::text_primary())
                })
            })
            // 展开着子面板的那一行常亮,鼠标挪进子面板后仍能看出是从哪一层进去的
            .when(highlighted, |el| {
                el.bg(ui::with_alpha(ui::accent(), 0.2))
                    .text_color(ui::text_primary())
            })
    }

    fn render_here_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.source.accepts_target(&self.dir);
        let label = if self.is_root() {
            t("fileTree", "moveTo.root")
        } else {
            t("fileTree", "moveTo.here")
        };
        let target = self.dir.clone();
        Self::row(
            SharedString::from(format!("move-to-here-{}", self.dir.display())),
            enabled,
            false,
        )
        .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
            if *hovered {
                this.set_open_child(None, cx);
            }
        }))
        .when(enabled, |el| {
            el.on_click(cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                this.pick(target.clone(), window, cx);
            }))
        })
        .child(
            FileIcon::folder(self.is_root())
                .size(px(14.0))
                .color(ui::color_folder()),
        )
        .child(div().flex_1().truncate().child(label))
        .into_any_element()
    }

    fn render_dir_row(
        &self,
        index: usize,
        entry: &FileEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = self.source.accepts_target(&entry.path);
        let is_open = self.open_child == Some(index);
        let target = entry.path.clone();
        let this = cx.entity();
        let icon = FileIcon::new(&entry.name, true, is_open).size(px(14.0));
        let icon = if entry.ignored {
            icon.color(ui::text_muted())
        } else {
            icon
        };
        Self::row(
            SharedString::from(format!("move-to-{}-{index}", self.dir.display())),
            enabled,
            is_open,
        )
        .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
            if !*hovered {
                return;
            }
            // 置灰的行(源自身 / 源的子孙)不展开:里面没有一个能落的目标
            this.set_open_child(enabled.then_some(index), cx);
        }))
        .when(enabled, |el| {
            el.on_click(cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                this.pick(target.clone(), window, cx);
            }))
        })
        .child(icon)
        .child(
            div()
                .flex_1()
                .truncate()
                .when(entry.ignored, |el| el.text_color(ui::text_muted()))
                .child(entry.name.clone()),
        )
        .when(enabled, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child("▸"),
            )
        })
        // 记下这一行的矩形,子面板下一帧按它定位(见模块注释「子面板挂在哪」)
        .child(
            canvas(
                move |bounds: Bounds<Pixels>, _window, cx| {
                    this.update(cx, |panel: &mut Self, _cx| {
                        panel.row_bounds.insert(index, bounds);
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
    }
}

impl Render for MoveToPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();

        let mut list = div()
            .id(SharedString::from(format!(
                "move-to-list-{}",
                self.dir.display()
            )))
            .flex()
            .flex_col()
            .max_h(px(MAX_LIST_HEIGHT))
            .overflow_y_scroll()
            .child(self.render_here_row(cx));
        match self.children.as_ref() {
            None => list = list.child(Self::hint(t("fileTree", "empty.loading"))),
            Some(Err(_)) => list = list.child(Self::hint(t("fileTree", "empty.loadFailed"))),
            Some(Ok(dirs)) if dirs.is_empty() => {
                list = list.child(Self::hint(t("fileTree", "moveTo.empty")));
            }
            Some(Ok(dirs)) => {
                list = list.child(div().h(px(1.0)).my(px(4.0)).bg(ui::border_subtle()));
                for (index, entry) in dirs.iter().enumerate() {
                    list = list.child(self.render_dir_row(index, entry, cx));
                }
            }
        }

        // 展开中的子面板:挂在面板根上(不在滚动容器里),纵向对齐到那一行。
        // 行矩形是上一帧记的 —— 悬停到 notify 之后的这一帧,行早已画过至少一次
        let child = self.open_child.and_then(|index| {
            let entry = self.subdirs().get(index)?;
            let panel = self.child_panels.get(&entry.path)?.clone();
            let row = self.row_bounds.get(&index)?;
            let root = self.root_bounds?;
            Some((panel, row.origin.y - root.origin.y))
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .min_w(px(180.0))
            .max_w(px(320.0))
            .p(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_overlay())
            .shadow_lg()
            .text_size(ui::font_px(12.0))
            // 面板挂在菜单项里,菜单面板已经 occlude 了;这里再挡一道,
            // 免得滚动条上的按下穿到底下去(与 branch_family 同)
            .occlude()
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, cx| {
                        this.update(cx, |panel: &mut Self, _cx| {
                            panel.root_bounds = Some(bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(list)
            .when_some(child, |el, (panel, top)| {
                // 与菜单基件的子菜单同一套坐标:父项右缘、上移 4px 对齐面板内边距,
                // 外套 `anchored` 白拿贴边收拢
                el.child(
                    div()
                        .absolute()
                        .left(relative(1.0))
                        .top(top - px(4.0))
                        .child(anchored().snap_to_window_with_margin(px(4.0)).child(panel)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, is_dir: bool) -> MoveSource {
        let path = PathBuf::from(path);
        MoveSource {
            parent: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path,
            is_dir,
            remote: false,
        }
    }

    fn remote(path: &str, is_dir: bool) -> MoveSource {
        MoveSource {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or_default().to_string(),
            is_dir,
            parent: PathBuf::from(crate::remote_ssh::parent_posix(path).unwrap_or_default()),
            remote: true,
        }
    }

    // 本地用例一律写正斜杠:Windows 上 `Path` 两种分隔符都认,Linux 上反斜杠只是
    // 普通字符(`D:\p\src\a.rs` 的 `parent()` 会是空),CI 的 ubuntu 跑过这里会红。

    /// 文件:除了「已在这个目录里」哪儿都能去 —— 包括同名目录的兄弟、更深的层。
    #[test]
    fn 文件只拒绝原目录() {
        let file = local("D:/p/src/a.rs", false);
        assert!(!file.accepts_target(Path::new("D:/p/src")), "已在该目录");
        assert!(file.accepts_target(Path::new("D:/p")));
        assert!(file.accepts_target(Path::new("D:/p/src/deep")));
        assert!(file.accepts_target(Path::new("D:/p/docs")));
    }

    /// 目录:原目录、自身、自身的子孙都不接;同名前缀的兄弟(`src-old`)不算子孙。
    #[test]
    fn 目录拒绝自身与子孙() {
        let dir = local("D:/p/src", true);
        assert!(!dir.accepts_target(Path::new("D:/p")), "已在该目录");
        assert!(!dir.accepts_target(Path::new("D:/p/src")), "自身");
        assert!(!dir.accepts_target(Path::new("D:/p/src/core")), "子孙");
        assert!(
            !dir.accepts_target(Path::new("D:/p/src/core/x")),
            "更深的子孙"
        );
        assert!(
            dir.accepts_target(Path::new("D:/p/src-old")),
            "同名前缀的兄弟"
        );
        assert!(dir.accepts_target(Path::new("D:/p/docs")));
    }

    /// 远程路径按 POSIX 字符串判,尾 `/` 不算差异;Windows 客户端上也不许把
    /// `Path` 的反斜杠语义带进来。
    #[test]
    fn 远程按_posix_判() {
        let dir = remote("/home/u/proj/src", true);
        assert!(!dir.accepts_target(Path::new("/home/u/proj")), "已在该目录");
        assert!(
            !dir.accepts_target(Path::new("/home/u/proj/")),
            "尾斜杠等价"
        );
        assert!(!dir.accepts_target(Path::new("/home/u/proj/src")), "自身");
        assert!(
            !dir.accepts_target(Path::new("/home/u/proj/src/core")),
            "子孙"
        );
        assert!(
            dir.accepts_target(Path::new("/home/u/proj/src2")),
            "同名前缀的兄弟"
        );
        assert!(dir.accepts_target(Path::new("/home/u/proj/docs")));

        let file = remote("/home/u/proj/src/a.rs", false);
        assert!(!file.accepts_target(Path::new("/home/u/proj/src")));
        assert!(file.accepts_target(Path::new("/home/u/proj/src/deep")));
        assert!(file.accepts_target(Path::new("/home/u/proj")));
    }

    #[test]
    fn posix_去尾斜杠_根保留() {
        assert_eq!(trim_posix("/a/b/"), "/a/b");
        assert_eq!(trim_posix("/a/b"), "/a/b");
        assert_eq!(trim_posix("/"), "/");
        assert_eq!(trim_posix(""), "/");
    }
}

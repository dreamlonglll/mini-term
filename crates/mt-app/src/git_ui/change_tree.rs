//! 改动文件的目录树:「更改」区树形视图、提交详情、提交 diff 左栏三处共用
//! (issue #84,对标 IDEA 改动浏览器的按目录分组)。
//!
//! 按 `/` 切路径建树,再做三件事:
//!
//! - **单链目录压缩**(IDEA 的 Compact Middle Packages):某目录只有一个子节点、且那个
//!   子节点也是目录时并成一个节点,显示名写成 `a/b/c`(与文件行弱化目录同一种分隔符)。
//!   叶子文件不参与合并 —— 只装着一个文件的目录照样单独一行;
//! - 每个目录带**递归文件数**(目录行名称后面弱化写「N 个文件」);
//! - 排序:目录在前、文件在后,各自按名称排(不分大小写)。
//!
//! 目录默认展开;折叠状态由调用方按节点**完整路径**记(压缩节点的完整路径是链上最深的
//! 那一级),变更区的 key 前面另带区名(同一路径在四个区各自独立折叠)。
//!
//! 拍平([`flatten_change_tree`])的结果就是界面上的行序:渲染、键盘导航、Shift 连选、
//! diff 弹窗里前后换文件都走它,视觉顺序与键盘顺序不会对不上。

use std::cmp::Ordering;

use gpui::{
    AnyElement, Div, ElementId, InteractiveElement, ParentElement, SharedString, Stateful, Styled,
    div, px,
};
use mt_ui::icons::FileIcon;

use super::{
    ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_LIST, ICON_LIST_TREE, ICON_SIZE, ICON_SIZE_SM,
    ROW_H, commit_file_status, file_label, hover_bg, icon, icon_button, list_row, status_letter,
};
use crate::i18n::{t, tr};
use crate::ui;

/// 每深一层的缩进。
pub const TREE_INDENT: f32 = 16.0;

/// 目录树的一个节点。`file` 是**下标**而不是拷贝 —— 建树只是换个排列方式,内容仍在
/// 调用方的原数组里。
#[derive(Debug, PartialEq)]
pub struct ChangeTreeNode {
    /// 显示名:文件是文件名;目录是目录名,压缩过的是 `a/b/c`。
    pub name: String,
    /// 仓库内完整路径(压缩过的目录是链上最深那一级的路径)。
    pub full_path: String,
    pub file: Option<usize>,
    /// 目录:递归文件数;文件:1。
    pub file_count: usize,
    pub children: Vec<ChangeTreeNode>,
}

impl ChangeTreeNode {
    fn is_dir(&self) -> bool {
        self.file.is_none()
    }
}

/// 按 `/` 切路径建目录树(单链压缩 + 文件数 + 排序,见模块注释)。`paths` 是
/// `(调用方的下标, 路径)`。
///
/// 同名的文件与目录(`build` 文件与 `build/out.js`)各是各的节点,不合并。
pub fn build_change_tree(paths: &[(usize, &str)]) -> Vec<ChangeTreeNode> {
    let mut root: Vec<ChangeTreeNode> = Vec::new();
    for (index, path) in paths {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = &mut root;
        let mut path_so_far = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                path_so_far.push('/');
            }
            path_so_far.push_str(part);
            if i == parts.len() - 1 {
                current.push(ChangeTreeNode {
                    name: (*part).to_string(),
                    full_path: path_so_far.clone(),
                    file: Some(*index),
                    file_count: 1,
                    children: Vec::new(),
                });
            } else {
                let pos = current.iter().position(|n| n.name == *part && n.is_dir());
                let pos = match pos {
                    Some(pos) => pos,
                    None => {
                        current.push(ChangeTreeNode {
                            name: (*part).to_string(),
                            full_path: path_so_far.clone(),
                            file: None,
                            file_count: 0,
                            children: Vec::new(),
                        });
                        current.len() - 1
                    }
                };
                current = &mut current[pos].children;
            }
        }
    }
    finish(&mut root);
    root
}

/// 自底向上:压缩单链、算文件数、排序。
fn finish(nodes: &mut [ChangeTreeNode]) {
    for node in nodes.iter_mut() {
        if !node.is_dir() {
            node.file_count = 1;
            continue;
        }
        finish(&mut node.children);
        while node.children.len() == 1 && node.children[0].is_dir() {
            let Some(child) = node.children.pop() else {
                break;
            };
            node.name = format!("{}/{}", node.name, child.name);
            node.full_path = child.full_path;
            node.children = child.children;
        }
        node.file_count = node.children.iter().map(|c| c.file_count).sum();
    }
    nodes.sort_by(compare_nodes);
}

/// 目录在前、文件在后;同类按名称不分大小写排,只差大小写的再按原样排(顺序稳定)。
fn compare_nodes(a: &ChangeTreeNode, b: &ChangeTreeNode) -> Ordering {
    b.is_dir()
        .cmp(&a.is_dir())
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

/// 拍平后的一行。
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeTreeRow {
    Dir {
        name: String,
        full_path: String,
        depth: usize,
        collapsed: bool,
        file_count: usize,
    },
    File {
        index: usize,
        depth: usize,
    },
}

/// 拍平成界面上的行序。`collapsed(完整路径)` 说这个目录收没收起(收起的不往下展开)。
pub fn flatten_change_tree(
    nodes: &[ChangeTreeNode],
    collapsed: &dyn Fn(&str) -> bool,
) -> Vec<ChangeTreeRow> {
    let mut out = Vec::new();
    flatten_into(nodes, collapsed, 0, &mut out);
    out
}

fn flatten_into(
    nodes: &[ChangeTreeNode],
    collapsed: &dyn Fn(&str) -> bool,
    depth: usize,
    out: &mut Vec<ChangeTreeRow>,
) {
    for node in nodes {
        match node.file {
            Some(index) => out.push(ChangeTreeRow::File { index, depth }),
            None => {
                let is_collapsed = collapsed(&node.full_path);
                out.push(ChangeTreeRow::Dir {
                    name: node.name.clone(),
                    full_path: node.full_path.clone(),
                    depth,
                    collapsed: is_collapsed,
                    file_count: node.file_count,
                });
                if !is_collapsed {
                    flatten_into(&node.children, collapsed, depth + 1, out);
                }
            }
        }
    }
}

/// 树形或平铺的行:`tree = false` 时按 `paths` 原顺序一个文件一行、不缩进(列表视图
/// 走同一套行模型)。
pub fn change_rows(
    paths: &[(usize, &str)],
    tree: bool,
    collapsed: &dyn Fn(&str) -> bool,
) -> Vec<ChangeTreeRow> {
    if tree {
        flatten_change_tree(&build_change_tree(paths), collapsed)
    } else {
        paths
            .iter()
            .map(|&(index, _)| ChangeTreeRow::File { index, depth: 0 })
            .collect()
    }
}

/// 行序里的文件下标(目录行跳过)。
pub fn row_file_order(rows: &[ChangeTreeRow]) -> Vec<usize> {
    rows.iter()
        .filter_map(|row| match row {
            ChangeTreeRow::File { index, .. } => Some(*index),
            ChangeTreeRow::Dir { .. } => None,
        })
        .collect()
}

/// 只画前 `max` 行时,后面藏起来了几个文件:文件行算一个,收起的目录算它的全部文件,
/// 展开的目录不算(它的文件就是后面那些行)。
pub fn hidden_file_count(rows: &[ChangeTreeRow], max: usize) -> usize {
    rows.iter()
        .skip(max)
        .map(|row| match row {
            ChangeTreeRow::File { .. } => 1,
            ChangeTreeRow::Dir {
                collapsed: true,
                file_count,
                ..
            } => *file_count,
            ChangeTreeRow::Dir { .. } => 0,
        })
        .sum()
}

/// 树形行的左内边距(`depth` 层缩进 + 行本身的 8)。
pub fn tree_row_pad(depth: usize) -> f32 {
    depth as f32 * TREE_INDENT + 8.0
}

/// 目录行名称后面那一截「N 个文件」。
pub fn dir_file_count_label(count: usize) -> String {
    if count == 1 {
        t("gitChanges", "dirFilesOne").to_string()
    } else {
        tr!("gitChanges", "dirFiles", count = count.to_string())
    }
}

/// 目录行:折叠箭头 + 文件夹图标 + 名称(压缩过的是 `a/b/c`)+ 弱化的文件数。整行可点,
/// 点击(切换收起 / 展开)由调用方挂。行高、缩进与文件行一致。
pub fn tree_dir_row(
    id: impl Into<ElementId>,
    name: impl Into<SharedString>,
    file_count: usize,
    depth: usize,
    collapsed: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_H))
        .pr(px(8.0))
        .pl(px(tree_row_pad(depth)))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_secondary())
        .hover(|el| el.bg(hover_bg()))
        .child(
            div()
                .flex_none()
                .w(px(ICON_SIZE))
                .flex()
                .justify_center()
                .child(icon(
                    if collapsed {
                        ICON_CHEVRON_RIGHT
                    } else {
                        ICON_CHEVRON_DOWN
                    },
                    ICON_SIZE_SM,
                    ui::text_muted(),
                )),
        )
        .child(FileIcon::folder(!collapsed).size(px(ICON_SIZE)))
        .child(div().min_w_0().truncate().child(name.into()))
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(dir_file_count_label(file_count)),
        )
}

/// 提交里的一行改动文件(提交详情、提交 diff 左栏):[`list_row`] + [`file_label`] + 右端
/// 状态字母。`status` 是 `CommitFileInfo.status`(`added` / `modified` / …)。
/// `depth = None` 是平铺列表(写弱化目录),`Some(层数)` 是树形(按层缩进、不写目录)。
pub fn commit_file_row_at(
    id: impl Into<ElementId>,
    path: &str,
    status: &str,
    selected: bool,
    depth: Option<usize>,
) -> Stateful<Div> {
    commit_file_row_with(id, path, status, selected, depth, None)
}

/// [`commit_file_row_at`],另在状态字母前挂一段附注(提交详情栏的增删行数)。
pub fn commit_file_row_with(
    id: impl Into<ElementId>,
    path: &str,
    status: &str,
    selected: bool,
    depth: Option<usize>,
    extra: Option<AnyElement>,
) -> Stateful<Div> {
    let (letter, color) = commit_file_status(status);
    list_row(id, selected)
        .pl(px(tree_row_pad(depth.unwrap_or(0))))
        .child(file_label(
            path,
            color,
            status == "deleted",
            depth.is_none(),
        ))
        .children(extra)
        .child(status_letter(letter, color))
}

/// 「列表 / 树形」切换的小图标按钮(区块标题行右侧)。画的是**点了会切到**的那种视图。
pub fn view_mode_toggle(id: impl Into<ElementId>, tree: bool) -> Stateful<Div> {
    icon_button(
        id,
        if tree { ICON_LIST } else { ICON_LIST_TREE },
        if tree {
            t("gitChanges", "switchToList")
        } else {
            t("gitChanges", "switchToTree")
        },
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(nodes: &[ChangeTreeNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.name.as_str()).collect()
    }

    /// 共用目录只建一个节点;子节点不止一个的目录不压缩;文件下标原样带着。
    #[test]
    fn 建树共用目录节点() {
        let paths = [(0, "src/a.rs"), (1, "src/b.rs"), (2, "README.md")];
        let tree = build_change_tree(&paths);
        assert_eq!(names(&tree), ["src", "README.md"], "目录在前");
        assert_eq!(tree[0].full_path, "src");
        assert!(tree[0].file.is_none());
        assert_eq!(tree[0].file_count, 2);
        assert_eq!(names(&tree[0].children), ["a.rs", "b.rs"]);
        assert_eq!(tree[0].children[0].full_path, "src/a.rs");
        assert_eq!(tree[0].children[0].file, Some(0));
        assert_eq!(tree[1].file, Some(2));
        assert_eq!(tree[1].file_count, 1);
    }

    /// 单链目录压缩成一个节点(`a/b/c`),完整路径是最深那一级;叶子文件不参与合并 ——
    /// 只装一个文件的目录照样单独一行。
    #[test]
    fn 单链目录压缩() {
        let tree = build_change_tree(&[(0, "a/b/c/d.rs")]);
        assert_eq!(names(&tree), ["a/b/c"]);
        assert_eq!(tree[0].full_path, "a/b/c");
        assert_eq!(tree[0].file_count, 1);
        assert_eq!(names(&tree[0].children), ["d.rs"]);

        // IDEA 截图那种:.claude/skills/scenarios 下面分出 infra 与 report
        let tree = build_change_tree(&[
            (0, ".claude/skills/scenarios/infra/x.md"),
            (1, ".claude/skills/scenarios/infra/deep/y.md"),
            (2, ".claude/skills/scenarios/report/z.md"),
        ]);
        assert_eq!(names(&tree), [".claude/skills/scenarios"]);
        assert_eq!(tree[0].file_count, 3);
        let scenarios = &tree[0].children;
        assert_eq!(names(scenarios), ["infra", "report"]);
        assert_eq!(scenarios[0].full_path, ".claude/skills/scenarios/infra");
        assert_eq!(scenarios[0].file_count, 2);
        // infra 下面有目录也有文件:不压缩;deep 只装一个文件:也不并进文件
        assert_eq!(names(&scenarios[0].children), ["deep", "x.md"]);
        assert_eq!(scenarios[0].children[0].file_count, 1);
        assert_eq!(names(&scenarios[0].children[0].children), ["y.md"]);
    }

    /// 文件数是递归的(子目录里的也算)。
    #[test]
    fn 目录文件数递归() {
        let tree = build_change_tree(&[
            (0, "src/ui/a.rs"),
            (1, "src/ui/b.rs"),
            (2, "src/core/c.rs"),
            (3, "src/main.rs"),
        ]);
        assert_eq!(tree[0].name, "src");
        assert_eq!(tree[0].file_count, 4);
        let src = &tree[0].children;
        assert_eq!(names(src), ["core", "ui", "main.rs"]);
        assert_eq!(src[0].file_count, 1);
        assert_eq!(src[1].file_count, 2);
    }

    /// 排序:目录在前、文件在后,各自按名称不分大小写;压缩节点按显示名排。
    #[test]
    fn 排序目录在前且不分大小写() {
        let tree = build_change_tree(&[
            (0, "b.txt"),
            (1, "Zeta/x.rs"),
            (2, "A.txt"),
            (3, "alpha/one/x.rs"),
            (4, "c.TXT"),
            (5, "Beta/x.rs"),
            (6, "Beta/y.rs"),
        ]);
        assert_eq!(
            names(&tree),
            ["alpha/one", "Beta", "Zeta", "A.txt", "b.txt", "c.TXT"]
        );
    }

    /// 同名的文件与目录各是各的节点(目录在前)。
    #[test]
    fn 同名目录与文件不合并() {
        let tree = build_change_tree(&[(0, "build"), (1, "build/out.js")]);
        assert_eq!(tree.len(), 2);
        assert!(tree[0].file.is_none(), "目录在前");
        assert_eq!(tree[0].children[0].full_path, "build/out.js");
        assert_eq!(tree[1].file, Some(0));
    }

    /// 拍平:目录默认展开;收起的目录按完整路径认、不往下展开;深度逐层 +1;
    /// 行序里的文件顺序就是树的显示顺序。
    #[test]
    fn 拍平与折叠() {
        let paths = [
            (0, "src/ui/a.rs"),
            (1, "src/ui/b.rs"),
            (2, "z.md"),
            (3, "src/m.rs"),
        ];
        let tree = build_change_tree(&paths);
        let rows = flatten_change_tree(&tree, &|_| false);
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert!(matches!(
            &rows[0],
            ChangeTreeRow::Dir { name, depth: 0, collapsed: false, file_count: 3, .. } if name == "src"
        ));
        assert!(matches!(&rows[1], ChangeTreeRow::Dir { name, depth: 1, .. } if name == "ui"));
        assert_eq!(rows[2], ChangeTreeRow::File { index: 0, depth: 2 });
        assert_eq!(row_file_order(&rows), [0, 1, 3, 2]);

        let rows = flatten_change_tree(&tree, &|p| p == "src/ui");
        assert_eq!(row_file_order(&rows), [3, 2], "收起的 ui 不展开");
        assert!(matches!(
            &rows[1],
            ChangeTreeRow::Dir {
                collapsed: true,
                file_count: 2,
                ..
            }
        ));
    }

    /// 截断:后面藏起来的文件数 —— 文件行算一个,收起的目录算全部,展开的目录不算。
    #[test]
    fn 截断后藏起来的文件数() {
        let paths = [(0, "a/x.rs"), (1, "a/y.rs"), (2, "b/z.rs"), (3, "b/w.rs")];
        let tree = build_change_tree(&paths);
        let rows = flatten_change_tree(&tree, &|p| p == "b");
        // a / x / y / b(收起,2 个)
        assert_eq!(rows.len(), 4);
        assert_eq!(hidden_file_count(&rows, 4), 0);
        assert_eq!(hidden_file_count(&rows, 3), 2);
        assert_eq!(hidden_file_count(&rows, 2), 3);
        assert_eq!(hidden_file_count(&rows, 0), 4, "展开的 a 本身不算");
        let flat: Vec<(usize, &str)> = (0..5).map(|i| (i, "f")).collect();
        assert_eq!(
            hidden_file_count(&change_rows(&flat, false, &|_| false), 3),
            2
        );
    }

    /// 平铺模式按原顺序一个文件一行。
    #[test]
    fn 平铺模式保持原顺序() {
        let paths = [(4, "z.rs"), (1, "a/b.rs")];
        let rows = change_rows(&paths, false, &|_| false);
        assert_eq!(row_file_order(&rows), [4, 1]);
        let rows = change_rows(&paths, true, &|_| false);
        assert_eq!(row_file_order(&rows), [1, 4], "树形:目录在前");
    }
}

//! 文件树的菜单构建(从 `file_tree` 平移):行/背景右键菜单、菜单项序。

use std::path::{Path, PathBuf};

use gpui::{ClipboardItem, Entity};

use crate::file_ops::{FileBackendIdentity, FileOperationContext};
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, show_prompt};
use crate::store::AppStore;

use super::move_to::{MoveSource, move_to_menu_item};
use super::ops::{
    choose_upload_paths, copy_to_file_clipboard, new_entry_prompt, open_entry_in_terminal,
    paste_file_clipboard, spawn_tree_op, start_download,
};
use super::{FileTree, Row};

// ─── 右键菜单 ─────────────────────────────────────────────────

/// 文件树右键菜单的**项序**。`None` = 分隔线。
///
/// 逐条对照 `FileTree.tsx:210-325`。「查看变更」(`ViewDiff`)在 V 批把
/// [`crate::git_diff::open_file_diff`] 建好之后补上,**条件与原版一字不差**:
/// 非目录、且这个文件在 git 状态表里有条目,前置一条分隔线接在最末。
/// 「移动到 ▸」(`MoveTo`)原版没有,GPUI 版补的,紧跟「重命名」——
/// 两者都是「换路径」的操作,本地/远程、文件/目录一律有。
/// 「Git 历史」(`GitHistory`,IDEA 的 Show History)也是 GPUI 版补的,与「查看变更」
/// 同组:本地、非目录、文件落在 git 仓库里就给 —— **不要求有改动**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileMenuAction {
    OpenWithDefault,
    CopyEntry,
    Paste,
    Download,
    UploadFiles,
    UploadFolder,
    CopyRelativePath,
    CopyAbsolutePath,
    RevealInFolder,
    OpenInTerminal,
    Rename,
    MoveTo,
    Delete,
    NewFile,
    NewFolder,
    ViewDiff,
    GitHistory,
}

pub(super) fn file_menu_actions(
    is_dir: bool,
    has_git_status: bool,
    in_git_repo: bool,
    remote: bool,
) -> Vec<Option<FileMenuAction>> {
    use FileMenuAction::*;
    let mut actions = Vec::new();
    if !remote && !is_dir {
        actions.push(Some(OpenWithDefault));
    }
    actions.push(Some(CopyEntry));
    if is_dir {
        actions.push(Some(Paste));
    }
    if remote {
        actions.push(Some(Download));
        if is_dir {
            actions.extend([Some(UploadFiles), Some(UploadFolder)]);
        }
    }
    actions.extend([None, Some(CopyRelativePath), Some(CopyAbsolutePath)]);
    if !remote {
        actions.push(Some(RevealInFolder));
    }
    actions.extend([
        Some(OpenInTerminal),
        None,
        Some(Rename),
        Some(MoveTo),
        Some(Delete),
    ]);
    if is_dir {
        actions.extend([None, Some(NewFile), Some(NewFolder)]);
    }
    // 目录没有单文件 diff 可看 —— 原版这条判定是 `entryGitStatus && !entry.isDir`。
    // 「Git 历史」与它同组:只要文件在仓库里就给(没改动也有历史)
    if !remote && !is_dir && (has_git_status || in_git_repo) {
        actions.push(None);
        if has_git_status {
            actions.push(Some(ViewDiff));
        }
        if in_git_repo {
            actions.push(Some(GitHistory));
        }
    }
    actions
}

/// 一行(文件/目录)的右键菜单。
pub(super) fn file_menu(
    tree: &Entity<FileTree>,
    store: &Entity<AppStore>,
    row: &Row,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    can_paste: bool,
) -> Vec<MenuEntry> {
    let root = context.root.clone();
    let remote = matches!(&context.backend, FileBackendIdentity::Remote { .. });
    let mut entries = Vec::new();
    // 只做 stat、不开仓库(菜单是右键当场在 UI 线程上建的),远程 / 目录不查
    let in_git_repo =
        !remote && !row.is_dir && mt_project::git::is_inside_git_repo(&root, &row.path);
    for action in file_menu_actions(row.is_dir, row.git.is_some(), in_git_repo, remote) {
        let Some(action) = action else {
            entries.push(menu::separator());
            continue;
        };
        let path = row.path.clone();
        let name = row.name.clone();
        let tree = tree.clone();
        let root = root.clone();
        let context = context.clone();
        let connection = connection.clone();
        // 父目录:重命名/删除之后要刷的是它;新建时刷的是目录自己
        let parent = if remote {
            mt_remote::parent_posix(&path.to_string_lossy())
                .map(PathBuf::from)
                .unwrap_or_else(|| root.clone())
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.clone())
        };

        entries.push(match action {
            FileMenuAction::OpenWithDefault => {
                menu::item(t("fileTree", "menu.openWithDefault"), move |_window, cx| {
                    fs_ops::open_external(fs_ops::ExternalOpen::DefaultApp, path.clone(), cx);
                })
            }
            FileMenuAction::CopyEntry => {
                let row = row.clone();
                let context = context.clone();
                menu::item(t("fileTree", "menu.copy"), move |_window, cx| {
                    copy_to_file_clipboard(&tree, &row, &context, cx);
                })
            }
            FileMenuAction::Paste => MenuItem::new(t("fileTree", "menu.paste"))
                .disabled(!can_paste)
                .on_click(move |window, cx| {
                    paste_file_clipboard(tree.clone(), context.clone(), path.clone(), window, cx);
                })
                .into(),
            FileMenuAction::Download => {
                menu::item(t("fileTree", "menu.download"), move |window, cx| {
                    start_download(
                        tree.clone(),
                        context.clone(),
                        vec![path.clone()],
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::UploadFiles => {
                menu::item(t("fileTree", "menu.uploadFiles"), move |window, cx| {
                    choose_upload_paths(
                        tree.clone(),
                        context.clone(),
                        path.clone(),
                        false,
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::UploadFolder => {
                menu::item(t("fileTree", "menu.uploadFolder"), move |window, cx| {
                    choose_upload_paths(
                        tree.clone(),
                        context.clone(),
                        path.clone(),
                        true,
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::CopyRelativePath => {
                let relative = if remote {
                    mt_remote::posix_relative(&root.to_string_lossy(), &path.to_string_lossy())
                        .unwrap_or_default()
                } else {
                    fs_ops::relative_path(&path.to_string_lossy(), &root.to_string_lossy())
                };
                menu::item(
                    t("fileTree", "menu.copyRelativePath"),
                    move |_window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()));
                    },
                )
            }
            FileMenuAction::CopyAbsolutePath => {
                let absolute = path.to_string_lossy().to_string();
                menu::item(
                    t("fileTree", "menu.copyAbsolutePath"),
                    move |_window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(absolute.clone()));
                    },
                )
            }
            FileMenuAction::RevealInFolder => {
                menu::item(t("fileTree", "menu.revealInFolder"), move |_window, cx| {
                    fs_ops::open_external(fs_ops::ExternalOpen::Reveal, path.clone(), cx);
                })
            }
            FileMenuAction::OpenInTerminal => {
                let context = context.clone();
                let store = store.clone();
                let is_dir = row.is_dir;
                menu::item(t("fileTree", "menu.openInTerminal"), move |window, cx| {
                    open_entry_in_terminal(
                        tree.clone(),
                        store.clone(),
                        context.clone(),
                        path.clone(),
                        is_dir,
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::Rename => {
                let is_dir = row.is_dir;
                menu::item(t("fileTree", "menu.rename"), move |window, cx| {
                    let (tree, root, path, parent) =
                        (tree.clone(), root.clone(), path.clone(), parent.clone());
                    let detach_before = is_dir.then(|| path.clone());
                    let old_name = name.clone();
                    let context = context.clone();
                    let connection = connection.clone();
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
                            let detach_before = detach_before.clone();
                            let context = context.clone();
                            let connection = connection.clone();
                            spawn_tree_op(
                                tree.clone(),
                                context,
                                Some(parent.clone()),
                                false,
                                detach_before,
                                t("fileTree", "operation.renaming").into(),
                                move || match connection {
                                    Some(conn) => mt_remote::rename_entry(
                                        &conn,
                                        &root.to_string_lossy(),
                                        &path.to_string_lossy(),
                                        &new_name,
                                    )
                                    .map(|_| None),
                                    None => mt_project::fs::rename_entry(&root, &path, &new_name)
                                        .map(|_| None)
                                        .map_err(|e| format!("{e:#}")),
                                },
                                window,
                                cx,
                            );
                        },
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::MoveTo => move_to_menu_item(
                tree,
                context,
                connection,
                MoveSource {
                    path,
                    name,
                    is_dir: row.is_dir,
                    parent,
                    remote,
                },
            ),
            FileMenuAction::Delete => {
                let is_dir = row.is_dir;
                MenuItem::new(t("fileTree", "menu.delete"))
                    .danger()
                    .on_click(move |window, cx| {
                        let (tree, root, path, parent) =
                            (tree.clone(), root.clone(), path.clone(), parent.clone());
                        let context = context.clone();
                        let connection = connection.clone();
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
                                    let connection = connection.clone();
                                    let operation_path = path.clone();
                                    spawn_tree_op(
                                        tree.clone(),
                                        context.clone(),
                                        Some(parent.clone()),
                                        false,
                                        Some(path.clone()),
                                        t("fileTree", "operation.deleting").into(),
                                        move || match connection {
                                            Some(conn) => mt_remote::delete_entry(
                                                &conn,
                                                &root.to_string_lossy(),
                                                &operation_path.to_string_lossy(),
                                            )
                                            .map(|_| None),
                                            None => {
                                                mt_project::fs::delete_entry(&root, &operation_path)
                                                    .map(|_| None)
                                                    .map_err(|e| format!("{e:#}"))
                                            }
                                        },
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
                let context = context.clone();
                let connection = connection.clone();
                menu::item(t("fileTree", "menu.newFile"), move |window, cx| {
                    new_entry_prompt(
                        tree.clone(),
                        context.clone(),
                        connection.clone(),
                        path.clone(),
                        false,
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::NewFolder => {
                let context = context.clone();
                let connection = connection.clone();
                menu::item(t("fileTree", "menu.newFolder"), move |window, cx| {
                    new_entry_prompt(
                        tree.clone(),
                        context.clone(),
                        connection.clone(),
                        path.clone(),
                        true,
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::ViewDiff => {
                // 原版 `DiffModal` 收的是 (projectPath, GitFileStatus):仓库那一侧
                // 传的就是**项目根**,文件那一侧是状态表里的相对路径 —— 照抄。
                // HEAD → 工作区:文件树里看的是「相对上次提交改了什么」,暂存与否都算
                let store = store.clone();
                let repo = root.to_string_lossy().to_string();
                let rel = row.rel.clone();
                let label = row.git.as_ref().map(|(l, _)| l.clone()).unwrap_or_default();
                menu::item(t("fileTree", "menu.viewDiff"), move |window, cx| {
                    // 单元素序列、不支持按块操作(HEAD → 工作区两侧都不是能写回的那一侧)
                    crate::git_diff::open_file_diff(
                        store.clone(),
                        repo.clone(),
                        vec![crate::git_diff::DiffFile {
                            path: rel.clone(),
                            side: crate::git_diff::DiffSide::Working,
                            status_label: label.clone(),
                            hunk_actions: false,
                        }],
                        0,
                        None,
                        window,
                        cx,
                    );
                })
            }
            FileMenuAction::GitHistory => {
                // 项目根 + 相对路径;所属仓库(可能在项目根之上 / 嵌套子仓库)由后端就近发现
                let store = store.clone();
                let rel = row.rel.clone();
                menu::item(t("gitLog", "fileHistory.menu"), move |window, cx| {
                    crate::git_file_history::open(
                        store.clone(),
                        root.clone(),
                        rel.clone(),
                        window,
                        cx,
                    );
                })
            }
        });
    }
    entries
}

pub(super) fn background_menu(
    tree: &Entity<FileTree>,
    store: &Entity<AppStore>,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    can_paste: bool,
) -> Vec<MenuEntry> {
    let root = context.root.clone();
    let remote = matches!(&context.backend, FileBackendIdentity::Remote { .. });
    let mut entries = vec![
        MenuItem::new(t("fileTree", "menu.paste"))
            .disabled(!can_paste)
            .on_click({
                let tree = tree.clone();
                let root = root.clone();
                let context = context.clone();
                move |window, cx| {
                    paste_file_clipboard(tree.clone(), context.clone(), root.clone(), window, cx);
                }
            })
            .into(),
    ];
    if remote {
        entries.extend([
            menu::item(t("fileTree", "menu.uploadFiles"), {
                let tree = tree.clone();
                let root = root.clone();
                let context = context.clone();
                move |window, cx| {
                    choose_upload_paths(
                        tree.clone(),
                        context.clone(),
                        root.clone(),
                        false,
                        window,
                        cx,
                    )
                }
            }),
            menu::item(t("fileTree", "menu.uploadFolder"), {
                let tree = tree.clone();
                let root = root.clone();
                let context = context.clone();
                move |window, cx| {
                    choose_upload_paths(
                        tree.clone(),
                        context.clone(),
                        root.clone(),
                        true,
                        window,
                        cx,
                    )
                }
            }),
        ]);
    }
    entries.push(menu::separator());
    // 空白处代表项目根:直接打开根目录本身(看到的就是树里这些文件),而不是行菜单
    // 那种「开父目录并选中」—— 后者项目列表右键已有。断链远程的根是 POSIX 路径,同样不给
    if matches!(&context.backend, FileBackendIdentity::Local) {
        entries.push(menu::item(t("fileTree", "menu.revealInFolder"), {
            let root = root.clone();
            move |_window, cx| {
                fs_ops::open_external(fs_ops::ExternalOpen::DefaultApp, root.clone(), cx);
            }
        }));
    }
    entries.push(menu::item(t("fileTree", "menu.openInTerminal"), {
        let tree = tree.clone();
        let store = store.clone();
        let context = context.clone();
        let root = root.clone();
        move |window, cx| {
            open_entry_in_terminal(
                tree.clone(),
                store.clone(),
                context.clone(),
                root.clone(),
                true,
                window,
                cx,
            )
        }
    }));
    entries.push(menu::separator());
    entries.extend([
        menu::item(t("fileTree", "menu.newFile"), {
            let tree = tree.clone();
            let context = context.clone();
            let connection = connection.clone();
            let root = root.clone();
            move |window, cx| {
                new_entry_prompt(
                    tree.clone(),
                    context.clone(),
                    connection.clone(),
                    root.clone(),
                    false,
                    window,
                    cx,
                )
            }
        }),
        menu::item(t("fileTree", "menu.newFolder"), {
            let tree = tree.clone();
            let context = context.clone();
            let connection = connection.clone();
            let root = root.clone();
            move |window, cx| {
                new_entry_prompt(
                    tree.clone(),
                    context.clone(),
                    connection.clone(),
                    root.clone(),
                    true,
                    window,
                    cx,
                )
            }
        }),
    ]);
    entries
}

/// 快捷键提示里的修饰键名(与 `search_modal` 那份同规则)。
pub(super) fn mod_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

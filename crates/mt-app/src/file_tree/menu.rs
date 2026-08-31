//! 文件树的菜单构建与头部动作能力位(从 `file_tree` 平移):行/背景右键菜单、
//! 菜单项序、[`HeaderActionCapabilities`]。

use std::path::{Path, PathBuf};

use gpui::{ClipboardItem, Entity};

use crate::file_ops::{FileBackendIdentity, FileClipboardEntry, FileOperationContext};
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, show_prompt};
use crate::store::AppStore;

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
    Delete,
    NewFile,
    NewFolder,
    ViewDiff,
}

pub(super) fn file_menu_actions(
    is_dir: bool,
    has_git_status: bool,
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
    actions.extend([Some(OpenInTerminal), None, Some(Rename), Some(Delete)]);
    if is_dir {
        actions.extend([None, Some(NewFile), Some(NewFolder)]);
    }
    // 目录没有单文件 diff 可看 —— 原版这条判定是 `entryGitStatus && !entry.isDir`
    if !remote && !is_dir && has_git_status {
        actions.extend([None, Some(ViewDiff)]);
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
    for action in file_menu_actions(row.is_dir, row.git.is_some(), remote) {
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
            crate::remote_ssh::parent_posix(&path.to_string_lossy())
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
                    let path = path.clone();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(err) = mt_project::editor::open_path_with_default_app(&path)
                            {
                                eprintln!("[files] 默认程序打开失败: {err:#}");
                            }
                        })
                        .detach();
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
                    crate::remote_ssh::posix_relative(
                        &root.to_string_lossy(),
                        &path.to_string_lossy(),
                    )
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
                                    Some(conn) => crate::remote_ssh::rename_entry(
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
                                            Some(conn) => crate::remote_ssh::delete_entry(
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
                // 工作区侧(staged=false)与原版一致:文件树里看的是「改了什么还没提交」
                let store = store.clone();
                let repo = root.to_string_lossy().to_string();
                let rel = row.rel.clone();
                let label = row.git.as_ref().map(|(l, _)| l.clone()).unwrap_or_default();
                menu::item(t("fileTree", "menu.viewDiff"), move |window, cx| {
                    crate::git_diff::open_file_diff(
                        store.clone(),
                        repo.clone(),
                        rel.clone(),
                        false,
                        label.clone(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HeaderActionCapabilities {
    pub(super) show_upload: bool,
    pub(super) mutations_enabled: bool,
    pub(super) paste_enabled: bool,
}

pub(super) fn header_action_capabilities(
    context: Option<&FileOperationContext>,
    operation_busy: bool,
    clipboard: Option<&FileClipboardEntry>,
) -> HeaderActionCapabilities {
    let connected = context.is_some_and(|context| {
        matches!(
            &context.backend,
            FileBackendIdentity::Local | FileBackendIdentity::Remote { .. }
        )
    });
    let show_upload = context
        .is_some_and(|context| matches!(&context.backend, FileBackendIdentity::Remote { .. }));
    let mutations_enabled = connected && !operation_busy;
    let paste_enabled = mutations_enabled
        && context.is_some_and(|context| {
            clipboard.is_some_and(|clipboard| clipboard.can_paste_into(context))
        });
    HeaderActionCapabilities {
        show_upload,
        mutations_enabled,
        paste_enabled,
    }
}

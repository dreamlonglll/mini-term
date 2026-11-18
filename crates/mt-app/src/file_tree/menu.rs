//! 文件树的菜单构建(从 `file_tree` 平移):行/背景右键菜单、菜单项序。

use gpui::{ClipboardItem, Entity};
use mt_project::editor::Editor;

use crate::file_ops::{FileBackendIdentity, FileOperationContext};
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::store::AppStore;

use super::move_to::{MoveSource, move_to_menu_item};
use super::ops::{
    EntryTarget, choose_upload_paths, confirm_delete_entry, copy_to_file_clipboard,
    new_entry_prompt, open_entry_in_terminal, paste_file_clipboard, rename_entry_prompt,
    start_download,
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
/// 「Git 历史」与「查看变更」同组，只给后台确认属于仓库的文件；不要求有改动。
/// 「在此目录搜索内容」(`SearchInDirectory`)同样是 GPUI 版补的,只给本地目录:
/// SSH 项目本来就不支持全局搜索(`search_modal::open` 那道拦截)。
/// 「用 {编辑器} 打开」(`OpenWithEditor`)也是 GPUI 版补的,紧跟「使用默认工具打开」,
/// 只给本地 / WSL 的文件;设置里一个外部编辑器都没配时这一项不画(见 [`file_menu`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileMenuAction {
    OpenWithDefault,
    OpenWithEditor,
    CopyEntry,
    Paste,
    Download,
    UploadFiles,
    UploadFolder,
    CopyRelativePath,
    CopyAbsolutePath,
    RevealInFolder,
    OpenInTerminal,
    SearchInDirectory,
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
    git_enabled: bool,
) -> Vec<Option<FileMenuAction>> {
    use FileMenuAction::*;
    let mut actions = Vec::new();
    if !remote && !is_dir {
        actions.extend([Some(OpenWithDefault), Some(OpenWithEditor)]);
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
    actions.push(Some(OpenInTerminal));
    if !remote && is_dir {
        actions.push(Some(SearchInDirectory));
    }
    actions.extend([None, Some(Rename), Some(MoveTo), Some(Delete)]);
    if is_dir {
        actions.extend([None, Some(NewFile), Some(NewFolder)]);
    }
    // 目录没有单文件 diff 可看 —— 原版这条判定是 `entryGitStatus && !entry.isDir`。
    // 「Git 历史」与它同组:只要文件在仓库里就给(没改动也有历史)
    if git_enabled && !is_dir && (has_git_status || in_git_repo) {
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

/// 行上的键盘动作(F2 = 重命名、Delete = 删除)能不能触发:**按右键菜单的项序判**——
/// 菜单里没有这一项的,键盘同样不触发,两边的可用性不会各说各话。
///
/// 后三个 git 相关的入参只决定「查看变更 / Git 历史」那一组,与重命名 / 删除无关,
/// 这里一律按「没有」传,免得为了判一个键去探仓库归属(本地那一路是同步 stat)。
pub(super) fn row_key_action_available(is_dir: bool, remote: bool, action: FileMenuAction) -> bool {
    file_menu_actions(is_dir, false, false, remote, false).contains(&Some(action))
}

/// 一行(文件/目录)的右键菜单。`editor` 是设置里配的外部编辑器(`fs_ops::configured_editor`),
/// 没配时不画「用 {编辑器} 打开」—— 那样它就只是另一个「使用默认工具打开」。
#[allow(clippy::too_many_arguments)]
pub(super) fn file_menu(
    tree: &Entity<FileTree>,
    store: &Entity<AppStore>,
    row: &Row,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    can_paste: bool,
    git_capability: (bool, bool),
    editor: Option<Editor>,
) -> Vec<MenuEntry> {
    let (git_enabled, in_git_repo) = git_capability;
    let root = context.root.clone();
    let remote = matches!(&context.backend, FileBackendIdentity::Remote { .. });
    // 父目录:重命名/删除/移动之后要刷的是它;新建时刷的是目录自己
    let target = EntryTarget::of_row(row, &root, remote);
    let mut entries = Vec::new();
    // 仓库归属由树的后台任务发现；右键构建菜单绝不发 SSH 请求。
    for action in file_menu_actions(
        row.is_dir,
        row.git.is_some(),
        in_git_repo,
        remote,
        git_enabled,
    ) {
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
        let parent = target.parent.clone();

        entries.push(match action {
            FileMenuAction::OpenWithDefault => {
                menu::item(t("fileTree", "menu.openWithDefault"), move |_window, cx| {
                    fs_ops::open_external(fs_ops::ExternalOpen::DefaultApp, path.clone(), cx);
                })
            }
            FileMenuAction::OpenWithEditor => {
                let Some(editor) = editor.clone() else {
                    continue;
                };
                let label = tr!(
                    "fileTree",
                    "header.openWithEditor",
                    editor = editor.name.clone()
                );
                menu::item(label, move |_window, cx| {
                    fs_ops::open_external(
                        fs_ops::ExternalOpen::Editor(Some(editor.clone())),
                        path.clone(),
                        cx,
                    );
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
            FileMenuAction::SearchInDirectory => {
                let store = store.clone();
                menu::item(
                    t("fileTree", "menu.searchInDirectory"),
                    move |window, cx| {
                        crate::search_modal::open_in_directory(
                            store.clone(),
                            path.clone(),
                            window,
                            cx,
                        );
                    },
                )
            }
            // 重命名 / 删除与行上的 F2 / Delete 走同一个函数(见 `FileTree::run_row_key_action`)
            FileMenuAction::Rename => {
                let target = target.clone();
                menu::item(t("fileTree", "menu.rename"), move |window, cx| {
                    rename_entry_prompt(
                        tree.clone(),
                        context.clone(),
                        connection.clone(),
                        target.clone(),
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
                let target = target.clone();
                MenuItem::new(t("fileTree", "menu.delete"))
                    .danger()
                    .on_click(move |window, cx| {
                        confirm_delete_entry(
                            tree.clone(),
                            context.clone(),
                            connection.clone(),
                            target.clone(),
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
                let project_id = context.project_id.clone();
                let rel = row.rel.clone();
                let label = row.git.as_ref().map(|(l, _)| l.clone()).unwrap_or_default();
                menu::item(t("fileTree", "menu.viewDiff"), move |window, cx| {
                    if remote
                        && !crate::git_backend::remote_git_enabled(store.read(cx), &project_id)
                    {
                        crate::git_backend::request_enable(
                            store.clone(),
                            project_id.clone(),
                            window,
                            cx,
                        );
                        return;
                    }
                    let repo = crate::git_backend::project_context(store.read(cx), &project_id);
                    // 单元素序列、不支持按块操作(HEAD → 工作区两侧都不是能写回的那一侧)
                    crate::git_diff::open_file_diff(
                        store.clone(),
                        repo.clone(),
                        vec![crate::git_diff::DiffFile {
                            path: rel.clone(),
                            old_path: None,
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
                let project_id = context.project_id.clone();
                menu::item(t("gitLog", "fileHistory.menu"), move |window, cx| {
                    if remote
                        && !crate::git_backend::remote_git_enabled(store.read(cx), &project_id)
                    {
                        crate::git_backend::request_enable(
                            store.clone(),
                            project_id.clone(),
                            window,
                            cx,
                        );
                        return;
                    }
                    let project = crate::git_backend::project_context(store.read(cx), &project_id);
                    crate::git_file_history::open(
                        store.clone(),
                        project.clone(),
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

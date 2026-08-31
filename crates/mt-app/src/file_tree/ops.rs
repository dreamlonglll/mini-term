//! 文件树的文件操作自由函数(从 `file_tree` 平移):preflight 三件套、
//! [`spawn_tree_op`]、应用内文件剪贴板的复制/粘贴、上传/下载全套、
//! 在终端打开、新建文件/文件夹。

use std::path::PathBuf;

use gpui::{App, Entity, PathPromptOptions, SharedString, Window};

use crate::file_ops::{
    FileBackendIdentity, FileClipboardEntry, FileOperationContext, entry_target_directory,
};
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::prompt::{show_alert, show_file_conflict_choice, show_prompt};
use crate::store::AppStore;

use super::{FileTree, Row, same_file_source};

/// 跑一件阻塞文件操作。状态和结果都绑定开始时的项目/连接/generation；切换项目后
/// 旧结果不会刷新新树。同一 FileTree 同时只接受一件 mutation/transfer。
fn begin_tree_preflight(
    tree: &Entity<FileTree>,
    context: &FileOperationContext,
    label: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let start_state = tree.update(cx, |tree, cx| {
        if tree.operation_context(cx).as_ref() != Some(context) {
            return None;
        }
        if tree.operation_busy {
            return Some(false);
        }
        tree.operation_busy = true;
        tree.operation_label = Some(label.to_string());
        tree.active_operation_context = Some(context.clone());
        tree.active_operation_suppressed_path = None;
        cx.notify();
        Some(true)
    });
    match start_state {
        Some(true) => true,
        Some(false) => {
            show_alert(
                t("fileTree", "operation.busyTitle"),
                t("fileTree", "operation.busyMessage"),
                window,
                cx,
            );
            false
        }
        None => false,
    }
}

fn finish_tree_preflight(
    tree: &Entity<FileTree>,
    context: &FileOperationContext,
    cx: &mut App,
) -> Option<bool> {
    tree.update(cx, |tree, cx| {
        if tree.active_operation_context.as_ref() != Some(context) {
            return None;
        }
        let context_matches = tree.operation_context(cx).as_ref() == Some(context);
        tree.operation_busy = false;
        tree.operation_label = None;
        tree.active_operation_context = None;
        tree.active_operation_suppressed_path = None;
        cx.notify();
        Some(context_matches)
    })
}

fn retain_tree_preflight_for_choice(
    tree: &Entity<FileTree>,
    context: &FileOperationContext,
    cx: &mut App,
) -> bool {
    tree.update(cx, |tree, cx| {
        if tree.active_operation_context.as_ref() != Some(context) {
            return false;
        }
        if tree.operation_context(cx).as_ref() != Some(context) {
            tree.operation_busy = false;
            tree.operation_label = None;
            tree.active_operation_context = None;
            tree.active_operation_suppressed_path = None;
            cx.notify();
            return false;
        }
        tree.operation_label = Some(t("fileTree", "conflict.title").to_string());
        cx.notify();
        true
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_tree_op(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    refresh_dir: Option<PathBuf>,
    expand: bool,
    detach_before: Option<PathBuf>,
    label: SharedString,
    op: impl FnOnce() -> Result<Option<String>, String> + Send + 'static,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let suppressed_path = detach_before.clone();
    let start_state = tree.update(cx, |tree, cx| {
        if tree.operation_context(cx).as_ref() != Some(&context) {
            return None;
        }
        if tree.operation_busy {
            return Some(false);
        }
        tree.operation_busy = true;
        tree.operation_label = Some(label.to_string());
        tree.active_operation_context = Some(context.clone());
        tree.active_operation_suppressed_path = suppressed_path.clone();
        cx.notify();
        Some(true)
    });
    match start_state {
        None => return false,
        Some(false) => {
            show_alert(
                t("fileTree", "operation.busyTitle"),
                t("fileTree", "operation.busyMessage"),
                window,
                cx,
            );
            return false;
        }
        Some(true) => {}
    }
    if let Some(path) = detach_before {
        tree.update(cx, |tree, cx| {
            if tree.operation_context(cx).as_ref() == Some(&context) {
                tree.suppressed_subtrees.insert(path.clone());
                tree.detach_subtree(&path);
                cx.notify();
            }
        });
    }
    let failed_refresh_dir = refresh_dir.clone();
    let task = cx.background_executor().spawn(async move { op() });
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok(summary) => {
                    let operation_owned = tree.update(cx, |tree, cx| {
                        let current = tree.operation_context(cx);
                        if tree.active_operation_context.as_ref() != Some(&context) {
                            return false;
                        }
                        let same_source = current
                            .as_ref()
                            .is_some_and(|current| same_file_source(current, &context));
                        tree.operation_busy = false;
                        tree.operation_label = None;
                        tree.active_operation_context = None;
                        tree.active_operation_suppressed_path = None;
                        if let Some(path) = suppressed_path.as_ref() {
                            if same_source {
                                tree.detach_subtree(path);
                            }
                            if same_source
                                && !expand
                                && let Some(project_id) = tree.current_project.clone()
                            {
                                let key = path.to_string_lossy().to_string();
                                tree.store.update(cx, |store, cx| {
                                    store.set_dir_expanded(&project_id, &key, false, cx)
                                });
                            }
                            tree.suppressed_subtrees.remove(path);
                        }
                        if same_source && let Some(refresh_dir) = refresh_dir {
                            if expand {
                                tree.ensure_expanded(refresh_dir, cx);
                            } else {
                                tree.reload_dir(refresh_dir, cx);
                            }
                        }
                        cx.notify();
                        true
                    });
                    if operation_owned && let Some(summary) = summary {
                        show_alert(
                            t("fileTree", "operation.completeTitle"),
                            summary,
                            window,
                            cx,
                        );
                    }
                }
                Err(err) => {
                    eprintln!("[files] 操作失败: {err}");
                    let operation_owned = tree.update(cx, |tree, cx| {
                        if tree.active_operation_context.as_ref() != Some(&context) {
                            return false;
                        }
                        let same_source = tree
                            .operation_context(cx)
                            .as_ref()
                            .is_some_and(|current| same_file_source(current, &context));
                        tree.operation_busy = false;
                        tree.operation_label = None;
                        tree.active_operation_context = None;
                        tree.active_operation_suppressed_path = None;
                        if let Some(path) = suppressed_path.as_ref() {
                            if same_source {
                                tree.detach_subtree(path);
                            }
                            tree.suppressed_subtrees.remove(path);
                        }
                        if same_source && let Some(failed_refresh_dir) = failed_refresh_dir {
                            tree.reload_dir(failed_refresh_dir, cx);
                        }
                        cx.notify();
                        true
                    });
                    if operation_owned {
                        show_alert(
                            t("fileTree", "operation.failedTitle"),
                            tr!("fileTree", "operation.failedMessage", error = err),
                            window,
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    true
}

fn operation_summary(summary: &crate::remote_ssh::FileOperationSummary) -> Option<String> {
    let mut text = tr!(
        "fileTree",
        "operation.summary",
        completed = summary.completed,
        skipped = summary.skipped,
        failed = summary.failed
    );
    if !summary.warnings.is_empty() {
        text.push_str("\n\n");
        text.push_str(&summary.warnings.join("\n"));
    }
    Some(text)
}

pub(super) fn copy_to_file_clipboard(
    tree: &Entity<FileTree>,
    row: &Row,
    expected_context: &FileOperationContext,
    cx: &mut App,
) {
    tree.update(cx, |tree, cx| {
        if tree.operation_context(cx).as_ref() != Some(expected_context) {
            return;
        }
        tree.file_clipboard = Some(FileClipboardEntry {
            project_id: expected_context.project_id.clone(),
            root: expected_context.root.clone(),
            backend: expected_context.backend.clone(),
            generation: expected_context.generation,
            source: row.path.clone(),
            is_dir: row.is_dir,
        });
        cx.notify();
    });
}

pub(super) fn paste_file_clipboard(
    tree: Entity<FileTree>,
    expected_context: FileOperationContext,
    target_dir: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let prepared = (tree.read(cx).operation_context(cx).as_ref() == Some(&expected_context))
        .then(|| {
            let clipboard = tree.read(cx).file_clipboard.clone()?;
            clipboard
                .can_paste_into(&expected_context)
                .then_some((expected_context, clipboard))
        })
        .flatten();
    let Some((context, clipboard)) = prepared else {
        show_alert(
            t("fileTree", "clipboard.unavailableTitle"),
            t("fileTree", "clipboard.unavailableMessage"),
            window,
            cx,
        );
        return;
    };
    if clipboard.would_copy_into_itself(&target_dir) {
        show_alert(
            t("fileTree", "clipboard.recursiveTitle"),
            t("fileTree", "clipboard.recursiveMessage"),
            window,
            cx,
        );
        return;
    }
    let source_name = clipboard.source.file_name().map(|name| name.to_os_string());
    let Some(source_name) = source_name else {
        return;
    };

    match &context.backend {
        FileBackendIdentity::Local => {
            let root = context.root.clone();
            let source = clipboard.source.clone();
            let destination = target_dir.join(source_name);
            spawn_tree_op(
                tree,
                context,
                Some(target_dir),
                true,
                None,
                t("fileTree", "operation.copying").into(),
                move || {
                    mt_project::fs::copy_entry(
                        &root,
                        &source,
                        &destination,
                        mt_project::fs::CopyConflictPolicy::KeepBoth,
                    )
                    .map(|_| None)
                    .map_err(|e| format!("{e:#}"))
                },
                window,
                cx,
            );
        }
        FileBackendIdentity::Remote { .. } => {
            let Some(conn) = tree.read(cx).remote_conn(cx) else {
                return;
            };
            let root = context.root.to_string_lossy().into_owned();
            let source = clipboard.source.to_string_lossy().into_owned();
            let target = target_dir.to_string_lossy().into_owned();
            spawn_tree_op(
                tree,
                context,
                Some(target_dir),
                true,
                None,
                t("fileTree", "operation.copying").into(),
                move || {
                    crate::remote_ssh::copy_entry_keep_both(&conn, &root, &source, &target)
                        .map(|(_, summary)| operation_summary(&summary))
                },
                window,
                cx,
            );
        }
        FileBackendIdentity::BrokenRemote => {}
    }
}

pub(super) fn open_entry_in_terminal(
    tree: Entity<FileTree>,
    store: Entity<AppStore>,
    context: FileOperationContext,
    path: PathBuf,
    is_dir: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
        return;
    }
    let cwd_path = entry_target_directory(&path, is_dir, &context.root);
    let cwd = (cwd_path != context.root).then(|| cwd_path.to_string_lossy().into_owned());
    let opened = store.update(cx, |store, cx| {
        if store.active_project_id.as_deref() != Some(context.project_id.as_str()) {
            return false;
        }
        store
            .new_terminal_with_cwd(&context.project_id, None, None, cwd, window, cx)
            .is_some()
    });
    if opened {
        crate::workbench_area::activate_terminal_page(window, cx);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_upload(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    conn: mt_config::SshConnection,
    target_dir: PathBuf,
    local_paths: Vec<PathBuf>,
    strategy: crate::remote_ssh::FileConflictStrategy,
    window: &mut Window,
    cx: &mut App,
) {
    let root = context.root.to_string_lossy().into_owned();
    let target = target_dir.to_string_lossy().into_owned();
    let detach_before = target_dir.clone();
    spawn_tree_op(
        tree,
        context,
        Some(target_dir),
        true,
        Some(detach_before),
        t("fileTree", "operation.uploading").into(),
        move || {
            crate::remote_ssh::upload_paths(&conn, &root, &target, &local_paths, strategy)
                .map(|summary| operation_summary(&summary))
        },
        window,
        cx,
    );
}

pub(super) fn start_upload(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    target_dir: PathBuf,
    local_paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    if local_paths.is_empty() {
        return;
    }
    if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
        return;
    }
    let FileBackendIdentity::Remote { .. } = &context.backend else {
        return;
    };
    let Some(conn) = tree.read(cx).remote_conn(cx) else {
        return;
    };
    if !begin_tree_preflight(
        &tree,
        &context,
        t("fileTree", "operation.checkingConflicts").into(),
        window,
        cx,
    ) {
        return;
    }
    let root = context.root.to_string_lossy().into_owned();
    let target = target_dir.to_string_lossy().into_owned();
    let scan_paths = local_paths.clone();
    let task = cx.background_executor().spawn(async move {
        crate::remote_ssh::upload_conflicts(&conn, &root, &target, &scan_paths)
            .map(|conflicts| (conn, conflicts))
    });
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok((conn, conflicts)) if conflicts.is_empty() => {
                    if finish_tree_preflight(&tree, &context, cx) != Some(true) {
                        return;
                    }
                    run_upload(
                        tree.clone(),
                        context.clone(),
                        conn,
                        target_dir.clone(),
                        local_paths.clone(),
                        crate::remote_ssh::FileConflictStrategy::KeepBoth,
                        window,
                        cx,
                    );
                }
                Ok((conn, conflicts)) => {
                    if !retain_tree_preflight_for_choice(&tree, &context, cx) {
                        return;
                    }
                    let choice_tree = tree.clone();
                    let choice_context = context.clone();
                    let cancel_tree = tree.clone();
                    let cancel_context = context.clone();
                    show_file_conflict_choice(
                        conflicts,
                        move |strategy, window, cx| {
                            if finish_tree_preflight(&choice_tree, &choice_context, cx)
                                != Some(true)
                            {
                                return;
                            }
                            run_upload(
                                choice_tree.clone(),
                                choice_context.clone(),
                                conn.clone(),
                                target_dir.clone(),
                                local_paths.clone(),
                                strategy,
                                window,
                                cx,
                            );
                        },
                        move |_window, cx| {
                            finish_tree_preflight(&cancel_tree, &cancel_context, cx);
                        },
                        window,
                        cx,
                    );
                }
                Err(error) => {
                    if finish_tree_preflight(&tree, &context, cx).is_some() {
                        show_alert(
                            t("fileTree", "operation.failedTitle"),
                            tr!("fileTree", "operation.failedMessage", error = error),
                            window,
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
}

pub(super) fn choose_upload_paths(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    target_dir: PathBuf,
    directories: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let prompt = cx.prompt_for_paths(PathPromptOptions {
        files: !directories,
        directories,
        multiple: !directories,
        prompt: Some(
            t(
                "fileTree",
                if directories {
                    "upload.chooseFolderTitle"
                } else {
                    "upload.chooseFilesTitle"
                },
            )
            .into(),
        ),
    });
    window
        .spawn(cx, async move |cx| {
            let selected = match prompt.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let detail = error.to_string();
                    let _ = cx.update(|window, cx| {
                        if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
                            return;
                        }
                        show_alert(t("fileTree", "operation.failedTitle"), detail, window, cx);
                    });
                    return;
                }
                Err(error) => {
                    let detail = error.to_string();
                    let _ = cx.update(|window, cx| {
                        if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
                            return;
                        }
                        show_alert(t("fileTree", "operation.failedTitle"), detail, window, cx);
                    });
                    return;
                }
            };
            let _ = cx.update(|window, cx| {
                if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
                    return;
                }
                start_upload(tree, context, target_dir, selected, window, cx)
            });
        })
        .detach();
}

#[allow(clippy::too_many_arguments)]
fn run_download(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    conn: mt_config::SshConnection,
    remote_paths: Vec<PathBuf>,
    download_dir: PathBuf,
    strategy: crate::remote_ssh::FileConflictStrategy,
    window: &mut Window,
    cx: &mut App,
) {
    let root = context.root.to_string_lossy().into_owned();
    spawn_tree_op(
        tree,
        context,
        None,
        false,
        None,
        t("fileTree", "operation.downloading").into(),
        move || {
            crate::remote_ssh::download_entries(
                &conn,
                &root,
                &remote_paths,
                &download_dir,
                strategy,
            )
            .map(|summary| {
                let mut message = operation_summary(&summary).unwrap_or_default();
                message.push_str("\n\n");
                message.push_str(&tr!(
                    "fileTree",
                    "operation.downloadLocation",
                    path = download_dir.display()
                ));
                Some(message)
            })
        },
        window,
        cx,
    );
}

pub(super) fn start_download(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    remote_paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
        return;
    }
    let FileBackendIdentity::Remote { .. } = &context.backend else {
        return;
    };
    let Some(conn) = tree.read(cx).remote_conn(cx) else {
        return;
    };
    let store = tree.read(cx).store.clone();
    let download_dir = match store.read(cx).config().resolved_download_dir() {
        Ok(path) => path,
        Err(error) => {
            show_alert(
                t("fileTree", "download.directoryErrorTitle"),
                format!("{error:#}"),
                window,
                cx,
            );
            return;
        }
    };
    if !begin_tree_preflight(
        &tree,
        &context,
        t("fileTree", "operation.checkingConflicts").into(),
        window,
        cx,
    ) {
        return;
    }
    let scan_dir = download_dir.clone();
    let scan_paths = remote_paths.clone();
    let task = cx
        .background_executor()
        .spawn(async move { crate::remote_ssh::download_conflicts(&scan_dir, &scan_paths) });
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok(conflicts) if conflicts.is_empty() => {
                    if finish_tree_preflight(&tree, &context, cx) != Some(true) {
                        return;
                    }
                    run_download(
                        tree,
                        context,
                        conn,
                        remote_paths,
                        download_dir,
                        crate::remote_ssh::FileConflictStrategy::KeepBoth,
                        window,
                        cx,
                    );
                }
                Ok(conflicts) => {
                    if !retain_tree_preflight_for_choice(&tree, &context, cx) {
                        return;
                    }
                    let choice_tree = tree.clone();
                    let choice_context = context.clone();
                    let cancel_tree = tree.clone();
                    let cancel_context = context.clone();
                    show_file_conflict_choice(
                        conflicts,
                        move |strategy, window, cx| {
                            if finish_tree_preflight(&choice_tree, &choice_context, cx)
                                != Some(true)
                            {
                                return;
                            }
                            run_download(
                                choice_tree.clone(),
                                choice_context.clone(),
                                conn.clone(),
                                remote_paths.clone(),
                                download_dir.clone(),
                                strategy,
                                window,
                                cx,
                            );
                        },
                        move |_window, cx| {
                            finish_tree_preflight(&cancel_tree, &cancel_context, cx);
                        },
                        window,
                        cx,
                    );
                }
                Err(error) => {
                    if finish_tree_preflight(&tree, &context, cx).is_some() {
                        show_alert(
                            t("fileTree", "operation.failedTitle"),
                            tr!("fileTree", "operation.failedMessage", error = error),
                            window,
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
}

/// 「新建文件 / 新建文件夹」:问名字 → 建 → 展开父目录并重列。
pub(super) fn new_entry_prompt(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
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
            let root = context.root.clone();
            let context = context.clone();
            let connection = connection.clone();
            let operation_dir = dir.clone();
            spawn_tree_op(
                tree.clone(),
                context,
                Some(dir.clone()),
                true,
                None,
                t("fileTree", "operation.creating").into(),
                move || match connection {
                    Some(conn) => crate::remote_ssh::create_entry(
                        &conn,
                        &root.to_string_lossy(),
                        &operation_dir.to_string_lossy(),
                        &name,
                        is_dir,
                    )
                    .map(|_| None),
                    None => {
                        let target = PathBuf::from(fs_ops::child_path(
                            &operation_dir.to_string_lossy(),
                            &name,
                        ));
                        if is_dir {
                            mt_project::fs::create_directory(&root, &target)
                        } else {
                            mt_project::fs::create_file(&root, &target)
                        }
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
}

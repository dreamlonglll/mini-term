//! 文件树的文件操作自由函数(从 `file_tree` 平移):preflight 三件套、
//! [`spawn_tree_op`]、应用内文件剪贴板的复制/粘贴、上传/下载全套、
//! 在终端打开、新建文件/文件夹、重命名 / 删除(右键菜单与行上的 F2 / Delete 共用)、
//! 移动(拖拽落点与「移动到」菜单共用)。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, Entity, PathPromptOptions, SharedString, Window};
use parking_lot::Mutex;

use crate::file_ops::{
    FileBackendIdentity, FileClipboardEntry, FileOperationContext, entry_target_directory,
};
use crate::file_viewer::LineTarget;
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::prompt::{Confirm, show_alert, show_file_conflict_choice, show_prompt};
use crate::store::AppStore;

use super::move_to::MoveSource;
use super::recycle;
use super::{FileTree, Row, same_file_source};

/// [`spawn_tree_op`] 跑完之后怎么刷树。
///
/// 老入口的四个位置参数原样收进来,再加两项只有「移动」要的旋钮 —— 它是唯一一件
/// **两个目录同时变**的操作(源父目录少一项、目标目录多一项)。
pub(super) struct TreeOpPlan {
    /// 完成后重列的目录;`expand` 为真时顺带展开它(粘贴/新建/上传/移动的目标目录)。
    pub refresh_dir: Option<PathBuf>,
    pub expand: bool,
    /// 操作期间脱挂(停监听、清缓存、屏蔽 watcher)的子树:删除/重命名/移动的源目录,
    /// 上传的目标目录。
    pub detach_before: Option<PathBuf>,
    /// 结束后把脱挂子树的展开态折掉 —— 源路径已不复存在(删除/重命名/移动)时用;
    /// 上传脱挂的是目标目录、结束后还要它展开,故不折。老入口按 `!expand` 推。
    pub collapse_detached: bool,
    /// 完成(或失败)后额外重列、**不展开**的目录:移动要同时刷源父目录与目标目录。
    pub extra_reload: Vec<PathBuf>,
    /// 成功落地之后再走的一步(新建文件后在工作区打开它)。**只在项目 / 根 / 后端仍是
    /// 开始时那一个时才跑**:中途切走了项目,旧项目里建出来的文件不该在新项目的页签区
    /// 里冒出来。失败时不跑。
    pub on_success: Option<AfterSuccess>,
    /// 成功落地之后**一定**要走的一步:已开的文件页签跟上这次改动(重命名 / 移动改指向,
    /// 删除关掉或标明,见 `workbench_area` 模块注释「页签跟随文件树」)。与
    /// [`Self::on_success`] 相反,**不看**结束时项目切没切走 —— 文件系统已经变了,页签又是
    /// 各项目各留各的。先于 `on_success` 跑。
    pub on_landed: Option<AfterSuccess>,
    /// 失败时代替通用「文件操作失败」弹窗的处理,返回 `true` 表示接手了(回收站拒收时改问
    /// 要不要永久删除)。只在这件操作仍归它、项目 / 根 / 后端也没变时才问。
    pub on_failure: Option<OnFailure>,
}

/// [`TreeOpPlan::on_success`] 那一步。
pub(super) type AfterSuccess = Box<dyn FnOnce(&mut Window, &mut App)>;

/// [`TreeOpPlan::on_failure`] 那一步。
pub(super) type OnFailure = Box<dyn FnOnce(&mut Window, &mut App) -> bool>;

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
    spawn_tree_op_planned(
        tree,
        context,
        TreeOpPlan {
            refresh_dir,
            expand,
            detach_before,
            collapse_detached: !expand,
            extra_reload: Vec::new(),
            on_success: None,
            on_landed: None,
            on_failure: None,
        },
        label,
        op,
        window,
        cx,
    )
}

/// [`spawn_tree_op`] 的完整版:刷新计划走 [`TreeOpPlan`]。
pub(super) fn spawn_tree_op_planned(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    plan: TreeOpPlan,
    label: SharedString,
    op: impl FnOnce() -> Result<Option<String>, String> + Send + 'static,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let TreeOpPlan {
        refresh_dir,
        expand,
        detach_before,
        collapse_detached,
        extra_reload,
        on_success,
        on_landed,
        on_failure,
    } = plan;
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
    let failed_extra_reload = extra_reload.clone();
    let task = cx.background_executor().spawn(async move { op() });
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok(summary) => {
                    // 两位:这次收尾还归不归这件操作、项目 / 根 / 后端是不是还是开始时那一个
                    let (operation_owned, same_source) = tree.update(cx, |tree, cx| {
                        let current = tree.operation_context(cx);
                        if tree.active_operation_context.as_ref() != Some(&context) {
                            return (false, false);
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
                                && collapse_detached
                                && let Some(project_id) = tree.current_project.clone()
                            {
                                let key = path.to_string_lossy().to_string();
                                tree.store.update(cx, |store, cx| {
                                    store.set_dir_expanded(&project_id, &key, false, cx)
                                });
                            }
                            tree.suppressed_subtrees.remove(path);
                        }
                        if same_source {
                            if let Some(refresh_dir) = refresh_dir {
                                if expand {
                                    tree.ensure_expanded(refresh_dir, cx);
                                } else {
                                    tree.reload_dir(refresh_dir, cx);
                                }
                            }
                            for dir in extra_reload {
                                tree.reload_dir(dir, cx);
                            }
                        }
                        cx.notify();
                        (true, same_source)
                    });
                    if operation_owned && let Some(summary) = summary {
                        show_alert(
                            t("fileTree", "operation.completeTitle"),
                            summary,
                            window,
                            cx,
                        );
                    }
                    if let Some(on_landed) = on_landed {
                        on_landed(window, cx);
                    }
                    if operation_owned
                        && same_source
                        && let Some(on_success) = on_success
                    {
                        on_success(window, cx);
                    }
                }
                Err(err) => {
                    tracing::warn!("[files] 操作失败: {err}");
                    let (operation_owned, same_source) = tree.update(cx, |tree, cx| {
                        if tree.active_operation_context.as_ref() != Some(&context) {
                            return (false, false);
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
                        if same_source {
                            if let Some(failed_refresh_dir) = failed_refresh_dir {
                                tree.reload_dir(failed_refresh_dir, cx);
                            }
                            for dir in failed_extra_reload {
                                tree.reload_dir(dir, cx);
                            }
                        }
                        cx.notify();
                        (true, same_source)
                    });
                    let handled = operation_owned
                        && same_source
                        && on_failure.is_some_and(|on_failure| on_failure(window, cx));
                    if operation_owned && !handled {
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

fn operation_summary(summary: &mt_remote::FileOperationSummary) -> Option<String> {
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
                    mt_remote::copy_entry_keep_both(&conn, &root, &source, &target)
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
    strategy: mt_remote::FileConflictStrategy,
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
            mt_remote::upload_paths(&conn, &root, &target, &local_paths, strategy)
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
        mt_remote::upload_conflicts(&conn, &root, &target, &scan_paths)
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
                        mt_remote::FileConflictStrategy::KeepBoth,
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
    strategy: mt_remote::FileConflictStrategy,
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
            mt_remote::download_entries(&conn, &root, &remote_paths, &download_dir, strategy).map(
                |summary| {
                    let mut message = operation_summary(&summary).unwrap_or_default();
                    message.push_str("\n\n");
                    message.push_str(&tr!(
                        "fileTree",
                        "operation.downloadLocation",
                        path = download_dir.display()
                    ));
                    Some(message)
                },
            )
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
        .spawn(async move { mt_remote::download_conflicts(&scan_dir, &scan_paths) });
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
                        mt_remote::FileConflictStrategy::KeepBoth,
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

/// 移动(换父目录、保留原名)。拖拽落点确认之后与「移动到」菜单点选都到这里。
///
/// 刷新计划:目标目录展开并重列(与粘贴同,让用户看见落点);源父目录重列;
/// 源是目录时操作期间脱挂它(旧路径下的监听与缓存作废,展开态折掉 —— 与重命名
/// 同一口径)。无效落点(已在该目录 / 目录进自身)由 [`MoveSource::accepts_target`]
/// 在这里最后兜一次,UI 层本该早就把它们判成不可落。
pub(super) fn start_move(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    source: MoveSource,
    target_dir: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    if !source.accepts_target(&target_dir) {
        return;
    }
    let on_landed = moved_path(&target_dir, &source.path, source.remote)
        .map(|to| follow_moved_hook(&context, source.path.clone(), to));
    let plan = TreeOpPlan {
        refresh_dir: Some(target_dir.clone()),
        expand: true,
        detach_before: source.is_dir.then(|| source.path.clone()),
        collapse_detached: true,
        extra_reload: vec![source.parent.clone()],
        on_success: None,
        on_landed,
        on_failure: None,
    };
    let root = context.root.clone();
    let path = source.path.clone();
    expect_path_change(&context, &path, cx);
    spawn_tree_op_planned(
        tree,
        context,
        plan,
        t("fileTree", "operation.moving").into(),
        move || match connection {
            Some(conn) => mt_remote::move_entry(
                &conn,
                &root.to_string_lossy(),
                &path.to_string_lossy(),
                &target_dir.to_string_lossy(),
            )
            .map(|_| None),
            None => mt_project::fs::move_entry(&root, &path, &target_dir)
                .map(|_| None)
                .map_err(|e| format!("{e:#}")),
        },
        window,
        cx,
    );
}

/// 拖拽落点的移动:先问一句再动。
///
/// gpui 的起拖阈值只有 2px(`dnd` 模块注释第 3 条),点一下手抖就是一次拖拽;
/// 松手落在邻行上就把文件搬走,太容易误伤 —— 与 VS Code 资源管理器的
/// `explorer.confirmDragAndDrop` 默认值同一取舍。菜单「移动到」是明确的意图,不问。
pub(super) fn confirm_move(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    source: MoveSource,
    target_dir: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    if tree.read(cx).operation_context(cx).as_ref() != Some(&context) {
        return;
    }
    if !source.accepts_target(&target_dir) {
        return;
    }
    let target_label = if target_dir == context.root {
        t("fileTree", "moveTo.root").to_string()
    } else if source.remote {
        mt_remote::posix_relative(
            &context.root.to_string_lossy(),
            &target_dir.to_string_lossy(),
        )
        .unwrap_or_else(|| target_dir.to_string_lossy().into_owned())
    } else {
        fs_ops::relative_path(
            &target_dir.to_string_lossy(),
            &context.root.to_string_lossy(),
        )
    };
    Confirm::new(
        t("fileTree", "dialog.moveTitle"),
        tr!(
            "fileTree",
            "dialog.moveConfirm",
            name = source.name.clone(),
            target = target_label
        ),
    )
    .ok_text(t("fileTree", "dialog.moveOk"))
    .open(
        move |window, cx| {
            start_move(
                tree.clone(),
                context.clone(),
                connection.clone(),
                source.clone(),
                target_dir.clone(),
                window,
                cx,
            );
        },
        window,
        cx,
    );
}

/// 行级「重命名」/「删除」的对象。右键菜单与行上的 F2 / Delete 都从这里拿参数,
/// 两条入口走的是同一个函数([`rename_entry_prompt`] / [`confirm_delete_entry`])。
#[derive(Clone)]
pub(super) struct EntryTarget {
    pub path: PathBuf,
    /// 行上显示的名字(压缩链是 `a/b/c` 那一串):重命名框的默认值、删除确认里的名字。
    pub name: String,
    pub is_dir: bool,
    /// 操作完重列的目录。
    pub parent: PathBuf,
}

impl EntryTarget {
    /// 一行 → 操作对象。父目录按后端切(远程 POSIX / 本地平台),切不出来就落项目根。
    pub fn of_row(row: &Row, root: &Path, remote: bool) -> Self {
        let parent = if remote {
            mt_remote::parent_posix(&row.path.to_string_lossy()).map(PathBuf::from)
        } else {
            row.path.parent().map(Path::to_path_buf)
        }
        .unwrap_or_else(|| root.to_path_buf());
        Self {
            path: row.path.clone(),
            name: row.name.clone(),
            is_dir: row.is_dir,
            parent,
        }
    }
}

/// 「重命名」:问新名字 → 改名 → 重列父目录。目录改名期间脱挂它的子树(旧路径下的
/// 监听与缓存作废,展开态折掉)。
pub(super) fn rename_entry_prompt(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    entry: EntryTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let EntryTarget {
        path,
        name: old_name,
        is_dir,
        parent,
    } = entry;
    let detach_before = is_dir.then(|| path.clone());
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
            let (root, path) = (context.root.clone(), path.clone());
            let connection = connection.clone();
            // 改名后的路径与树上的行同一种拼法(远程 POSIX、本地按父目录自己的分隔符),
            // 已开的页签按它改指向
            let renamed = renamed_path(&parent, &new_name, connection.is_some());
            let plan = TreeOpPlan {
                refresh_dir: Some(parent.clone()),
                expand: false,
                detach_before: detach_before.clone(),
                collapse_detached: true,
                extra_reload: Vec::new(),
                on_success: None,
                on_landed: Some(follow_moved_hook(&context, path.clone(), renamed)),
                on_failure: None,
            };
            expect_path_change(&context, &path, cx);
            spawn_tree_op_planned(
                tree.clone(),
                context.clone(),
                plan,
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
}

/// 删除走哪条路。
#[derive(Clone)]
enum DeleteMode {
    /// SSH 远程项目:远端没有回收站,照旧硬删(确认框写明无法撤销)。连接快照三百多字节,
    /// 装箱免得另外两种也按它的尺寸占位。
    Remote(Box<mt_config::SshConnection>),
    /// 本地:移到系统回收站(见 [`recycle`] 模块注释)。
    Trash,
    /// 本地永久删除:回收站进不去(WSL / 网络盘 / 回收失败)、用户又点了「永久删除」之后才走。
    Permanent,
}

/// 「删除」:先确认,确认后删掉并重列父目录。删除期间脱挂目标子树,免得大目录逐文件的
/// 监听事件洪泛。
///
/// - SSH 远程项目:原样硬删,确认框说「无法撤销」;
/// - 本地项目:默认**移到回收站**,确认框说的是回收站;位置本来就没有回收站(WSL / UNC /
///   网络盘,见 [`recycle::recycle_support`])时不先试,直接问「无法移到回收站,是否永久
///   删除?」;试了被拒([`recycle::TrashFailure::Refused`])同样回到这一问 —— 绝不静默硬删。
///
/// 删除成功后已开的文件页签跟上(干净的关掉、有改动的留着并标明,见
/// `workbench_area::follow_deleted`)。
pub(super) fn confirm_delete_entry(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    connection: Option<mt_config::SshConnection>,
    entry: EntryTarget,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(connection) = connection {
        let (title, message) = if entry.is_dir {
            (
                t("fileTree", "dialog.deleteFolderTitle"),
                tr!(
                    "fileTree",
                    "dialog.deleteConfirmFolder",
                    name = entry.name.clone()
                ),
            )
        } else {
            (
                t("fileTree", "dialog.deleteFileTitle"),
                tr!(
                    "fileTree",
                    "dialog.deleteConfirmFile",
                    name = entry.name.clone()
                ),
            )
        };
        let mode = DeleteMode::Remote(Box::new(connection));
        Confirm::new(title, message)
            .ok_text(t("fileTree", "dialog.deleteOk"))
            .cancel_text(t("fileTree", "dialog.deleteCancel"))
            .open(
                move |window, cx| {
                    spawn_delete(
                        tree.clone(),
                        context.clone(),
                        entry.clone(),
                        mode.clone(),
                        window,
                        cx,
                    );
                },
                window,
                cx,
            );
        return;
    }
    if recycle::recycle_support(&entry.path) == recycle::RecycleSupport::Unsupported {
        confirm_permanent_delete(tree, context, entry, None, window, cx);
        return;
    }
    let (title, message) = if entry.is_dir {
        (
            t("fileTree", "dialog.trashFolderTitle"),
            tr!(
                "fileTree",
                "dialog.trashConfirmFolder",
                name = entry.name.clone()
            ),
        )
    } else {
        (
            t("fileTree", "dialog.trashFileTitle"),
            tr!(
                "fileTree",
                "dialog.trashConfirmFile",
                name = entry.name.clone()
            ),
        )
    };
    Confirm::new(title, message)
        .ok_text(t("fileTree", "dialog.trashOk"))
        .cancel_text(t("fileTree", "dialog.deleteCancel"))
        .open(
            move |window, cx| {
                spawn_delete(
                    tree.clone(),
                    context.clone(),
                    entry.clone(),
                    DeleteMode::Trash,
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
}

/// 「无法移到回收站,是否永久删除?」。`refused` 是回收站拒收的原因(试过了);`None` =
/// 位置本来就没有回收站(没试)。确认后才走原来的硬删。
fn confirm_permanent_delete(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    entry: EntryTarget,
    refused: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let message = match refused {
        Some(error) => tr!(
            "fileTree",
            "dialog.permanentDeleteFailed",
            name = entry.name.clone(),
            error = error
        ),
        None => tr!(
            "fileTree",
            "dialog.permanentDeleteUnsupported",
            name = entry.name.clone()
        ),
    };
    Confirm::new(t("fileTree", "dialog.permanentDeleteTitle"), message)
        .ok_text(t("fileTree", "dialog.permanentDeleteOk"))
        .cancel_text(t("fileTree", "dialog.deleteCancel"))
        .danger()
        .open(
            move |window, cx| {
                spawn_delete(
                    tree.clone(),
                    context.clone(),
                    entry.clone(),
                    DeleteMode::Permanent,
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
}

/// 真正删:三种 [`DeleteMode`] 共用同一份刷新计划与页签跟随。回收站拒收时由
/// [`TreeOpPlan::on_failure`] 接手,改问要不要永久删除。
fn spawn_delete(
    tree: Entity<FileTree>,
    context: FileOperationContext,
    entry: EntryTarget,
    mode: DeleteMode,
    window: &mut Window,
    cx: &mut App,
) {
    let root = context.root.clone();
    let path = entry.path.clone();
    // 回收站拒收的原因:后台那一步写、失败回调来取(拿得到 = 是回收站没收,不是校验没过)
    let refused: Arc<Mutex<Option<String>>> = Arc::default();
    let on_failure = matches!(mode, DeleteMode::Trash).then(|| {
        let (tree, context, entry, refused) = (
            tree.clone(),
            context.clone(),
            entry.clone(),
            refused.clone(),
        );
        Box::new(move |window: &mut Window, cx: &mut App| {
            let Some(error) = refused.lock().take() else {
                return false;
            };
            confirm_permanent_delete(tree, context, entry, Some(error), window, cx);
            true
        }) as OnFailure
    });
    let label = match mode {
        DeleteMode::Trash => t("fileTree", "operation.trashing"),
        DeleteMode::Remote(_) | DeleteMode::Permanent => t("fileTree", "operation.deleting"),
    };
    let plan = TreeOpPlan {
        refresh_dir: Some(entry.parent.clone()),
        expand: false,
        detach_before: Some(entry.path.clone()),
        collapse_detached: true,
        extra_reload: Vec::new(),
        on_success: None,
        on_landed: Some(follow_deleted_hook(&context, entry.path.clone())),
        on_failure,
    };
    expect_path_change(&context, &path, cx);
    spawn_tree_op_planned(
        tree,
        context,
        plan,
        label.into(),
        move || match mode {
            DeleteMode::Remote(conn) => {
                mt_remote::delete_entry(&conn, &root.to_string_lossy(), &path.to_string_lossy())
                    .map(|_| None)
            }
            DeleteMode::Trash => match recycle::move_to_trash(&root, &path) {
                Ok(()) => Ok(None),
                Err(recycle::TrashFailure::Invalid(error)) => Err(error),
                Err(recycle::TrashFailure::Refused(error)) => {
                    *refused.lock() = Some(error.clone());
                    Err(error)
                }
            },
            DeleteMode::Permanent => mt_project::fs::delete_entry(&root, &path)
                .map(|_| None)
                .map_err(|e| format!("{e:#}")),
        },
        window,
        cx,
    );
}

// ─── 页签跟随文件树(接线见 `workbench_area` 模块注释「页签跟随文件树」) ────

/// 重命名 / 移动落地后,已开的文件页签改指向新路径。
fn follow_moved_hook(context: &FileOperationContext, from: PathBuf, to: PathBuf) -> AfterSuccess {
    let (project_id, backend) = (context.project_id.clone(), context.backend.clone());
    Box::new(move |window, cx| {
        crate::workbench_area::follow_moved(&project_id, &backend, &from, &to, window, cx);
    })
}

/// 删除落地后,已开的文件页签跟上:干净的关掉,有改动的留着并标明「文件已被删除」。
fn follow_deleted_hook(context: &FileOperationContext, path: PathBuf) -> AfterSuccess {
    let (project_id, backend) = (context.project_id.clone(), context.backend.clone());
    Box::new(move |window, cx| {
        crate::workbench_area::follow_deleted(&project_id, &backend, &path, window, cx);
    })
}

/// 改动马上落地:接下来那一小会儿,受影响页签收到的监听事件是这次操作自己的回声。不先
/// 打招呼的话,干净的页签会抢在改指向之前按旧路径重载成错误页(光标、滚动全丢),
/// 有改动的会挂出「已被外部修改」。
fn expect_path_change(context: &FileOperationContext, path: &Path, cx: &mut App) {
    crate::workbench_area::expect_path_change(&context.project_id, &context.backend, path, cx);
}

/// 重命名后的路径:与树上的行同一种拼法 —— 远程按 POSIX,本地按父目录自己的分隔符
/// (与新建文件同一口径,见 [`new_entry_prompt`])。
fn renamed_path(parent: &Path, new_name: &str, remote: bool) -> PathBuf {
    let parent = parent.to_string_lossy();
    if remote {
        PathBuf::from(mt_remote::join_posix(&parent, new_name))
    } else {
        PathBuf::from(fs_ops::child_path(&parent, new_name))
    }
}

/// 移动后的路径:目标目录 + 原名。原名取路径最后一段(远程按 `/` 切,本地按平台),
/// 不取行上的显示名 —— 压缩链的显示名是 `a/b/c` 一串,移动的只是链尾那一个目录。
fn moved_path(target_dir: &Path, source: &Path, remote: bool) -> Option<PathBuf> {
    let name = if remote {
        let text = source.to_string_lossy();
        let leaf = text.trim_end_matches('/').rsplit('/').next()?.to_string();
        (!leaf.is_empty()).then_some(leaf)?
    } else {
        source.file_name()?.to_string_lossy().into_owned()
    };
    Some(renamed_path(target_dir, &name, remote))
}

/// 「新建文件 / 新建文件夹」:问名字 → 建 → 展开父目录并重列。
///
/// 建的是**文件**时随即在工作区打开它、焦点交给编辑器 —— 新建一个文件多半就是要往里
/// 写,还得回树里再点一下是白跑一趟。文件夹照旧只刷树。打开挂在 [`TreeOpPlan::on_success`]
/// 上:失败不开,建的这一会儿切走了项目也不开。
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
            let connection = connection.clone();
            let operation_dir = dir.clone();
            // 新条目的路径与树上的行同一种拼法:远程按 POSIX、本地按目录自身的分隔符。
            // 远程不用 `create_entry` 交回的那条 —— 它拼在规范化(跟随链接)之后的父目录上,
            // 与树上的行路径可能对不上,打开后高亮就落不到这一行
            let target = if connection.is_some() {
                PathBuf::from(mt_remote::join_posix(&dir.to_string_lossy(), &name))
            } else {
                PathBuf::from(fs_ops::child_path(&dir.to_string_lossy(), &name))
            };
            let operation_target = target.clone();
            let on_success = (!is_dir).then(|| {
                let store = tree.read(cx).store.clone();
                let open = move |window: &mut Window, cx: &mut App| {
                    // 定到第 1 行:Markdown / HTML 默认开渲染稿,空文件的渲染稿上没东西可看,
                    // 焦点也只会停在页面容器上;带行号打开的直接进源码、焦点进编辑器
                    crate::workbench_area::open_active_file(
                        store,
                        target,
                        Some(LineTarget::line(1)),
                        window,
                        cx,
                    );
                };
                Box::new(open) as AfterSuccess
            });
            spawn_tree_op_planned(
                tree.clone(),
                context.clone(),
                TreeOpPlan {
                    refresh_dir: Some(dir.clone()),
                    expand: true,
                    detach_before: None,
                    collapse_detached: false,
                    extra_reload: Vec::new(),
                    on_success,
                    on_landed: None,
                    on_failure: None,
                },
                t("fileTree", "operation.creating").into(),
                move || match connection {
                    Some(conn) => mt_remote::create_entry(
                        &conn,
                        &root.to_string_lossy(),
                        &operation_dir.to_string_lossy(),
                        &name,
                        is_dir,
                    )
                    .map(|_| None),
                    None => if is_dir {
                        mt_project::fs::create_directory(&root, &operation_target)
                    } else {
                        mt_project::fs::create_file(&root, &operation_target)
                    }
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
}

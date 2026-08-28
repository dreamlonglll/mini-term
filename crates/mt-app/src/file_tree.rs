//! 中栏:文件树。对应 `src/components/FileTree.tsx` 的主干。
//!
//! - 列目录走 [`crate::remote_ssh::list_directory_for`] —— 它是**唯一的本地/远程
//!   分流开关**:本地项目转 [`mt_project::fs::list_directory`](mt_project::fs::list_directory)
//!   (`.gitignore` 过滤与排序都在那边,这里不重复实现),SSH 远程项目走 SFTP
//!   readdir。两条路返回同一个 `FileEntry`,所以整棵树共用同一段加载代码,
//!   不会出现「树顶刷新走了本地、展开子目录走了远程」这类半截状态。
//!   两条都是**阻塞**函数,一律丢 background executor,不能在主线程上跑;
//!
//! # SSH 远程项目的四条差异(逐条对照 `FileTree.tsx:432-508`)
//!
//! 1. **不注册 notify watcher**(远端文件系统本机监听不到);
//! 2. **不拉 git 状态**(远程 Git 是二期);
//! 3. **不做单链目录压缩** —— 逐级 SFTP 往返太贵,原版原话「保持原样」;
//! 4. **不探子工程技术栈**(`ensure_dir_kinds` 是本机 `stat`)。
//!
//! 断链(连接被删)不去读本机同名路径:直接给
//! `fileTree.remote.broken` 那句明确错误(项目仍可见、可删)。
//! - 目录变化走 [`mt_project::watch::FsWatcher`](mt_project::watch::FsWatcher):
//!   sink 里往 channel 丢,主线程上的前台任务醒来后失效缓存并重列 ——
//!   与 AI 状态、终端重绘是同一套跨线程唤醒模式;
//! - 单击文件开[文件预览器](crate::file_viewer)(AA 批之前是双击调外部编辑器 ——
//!   预览器缺位时的临时替身,原版文件行上只有预览这一条路)。
//!
//! 文件拖进终端(把路径当文本写进 PTY)走 gpui 原生 drag:这边只在行上挂
//! [`on_drag`](gpui::StatefulInteractiveElement::on_drag) 交出
//! [`crate::dnd::DragFilePath`],落点与写入在 `terminal_area.rs`。
//!
//! # git 状态着色(Y 批)
//!
//! 数据是 [`mt_project::git::get_git_status`](mt_project::git::get_git_status)
//! (阻塞,丢后台),键为**以 `/` 分隔的相对路径**,与 `FileTree.tsx:496-507` 同构。
//! 刷新时机照抄原版四条:切项目 / `fs-change`(500ms 去抖)/ 终端里跑过 git 命令
//! (同一个去抖)/ 头部刷新按钮。第三条走 [`crate::git_watch`] 的**输出旁路**,
//! 本模块是它的第二个订阅者([`git_watch::Subscriber::FileTree`])——
//! `isAiPty` 那道闸在旁路里,AI pane 刷屏带不起这边的刷新。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, ClipboardItem, Context, DragMoveEvent, Entity, ExternalPaths, Global, Hsla,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, PathPromptOptions,
    Render, SharedString, StatefulInteractiveElement, Styled, Task, Window, div,
    prelude::FluentBuilder, px,
};
use mt_project::fs::FileEntry;
use mt_project::watch::FsWatcher;
use mt_ui::icons::FileIcon;
use mt_ui::icons::vector::{Geom, Ink, Shape, VectorIcon};
use mt_ui::tooltip::Tooltip;

use crate::file_ops::{
    FileBackendIdentity, FileClipboardEntry, FileOperationContext, entry_target_directory,
};
use crate::fs_ops;
use crate::git_watch;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, show_alert, show_file_conflict_choice, show_prompt};
use crate::store::AppStore;
use crate::ui;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalDropTarget {
    hit_id: String,
    target_dir: PathBuf,
}

struct GlobalFileTree(Entity<FileTree>);
impl Global for GlobalFileTree {}

/// 安装文件管理器的统一操作入口。文件页中的远程二进制/超限提示通过它复用
/// 现有下载冲突检查、下载目录设置和 busy ownership，不另建一套下载流程。
pub fn install(tree: Entity<FileTree>, cx: &mut App) {
    cx.set_global(GlobalFileTree(tree));
}

fn show_download_context_changed(project_id: &str, cx: &mut App) {
    let project_name = AppStore::global(cx)
        .read(cx)
        .project(project_id)
        .map(|project| project.name.clone())
        .unwrap_or_else(|| project_id.to_string());
    crate::toast::push_message(
        crate::notify::ToastKind::PasteError,
        project_id.to_string(),
        project_name,
        t("fileTree", "download.contextChanged").to_string(),
        cx,
    );
}

fn remote_download_context_matches(
    context: &FileOperationContext,
    project_id: &str,
    project_root: &str,
    connection_id: &str,
    connection_fingerprint: u64,
) -> bool {
    context.project_id == project_id
        && context.root.as_path() == Path::new(project_root)
        && matches!(
            &context.backend,
            FileBackendIdentity::Remote {
                connection_id: current_id,
                connection_fingerprint: current_fingerprint,
            } if current_id == connection_id
                && *current_fingerprint == connection_fingerprint
        )
}

/// 从当前可见文件页下载一个远程文件。项目或连接上下文已经变化时拒绝并提示，
/// 避免旧页签借当前文件树的连接把同名路径下载自另一台主机。
pub fn download_remote_file(
    project_id: &str,
    project_root: &str,
    connection_id: &str,
    connection_fingerprint: u64,
    path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(tree) = cx
        .try_global::<GlobalFileTree>()
        .map(|global| global.0.clone())
    else {
        show_download_context_changed(project_id, cx);
        return;
    };
    let context = {
        let tree = tree.read(cx);
        tree.operation_context(cx)
    };
    let Some(context) = context else {
        show_download_context_changed(project_id, cx);
        return;
    };
    if !remote_download_context_matches(
        &context,
        project_id,
        project_root,
        connection_id,
        connection_fingerprint,
    ) {
        show_download_context_changed(project_id, cx);
        return;
    }
    start_download(tree, context, vec![path], window, cx);
}

pub struct FileTree {
    store: Entity<AppStore>,
    /// 已列出的目录 → 子项。
    entries: HashMap<PathBuf, Vec<FileEntry>>,
    /// 正在列的目录(防重复排队)。
    loading: HashSet<PathBuf>,
    /// 每个目录当前有效的请求号；删除/切换时移除即可丢弃迟到结果。
    dir_request_ids: HashMap<PathBuf, u64>,
    next_dir_request_id: u64,
    /// 文件操作或 watcher 命中正在加载的目录时，当前请求完成后必须再列一次。
    /// bool 表示那次补列是否需要强制刷新远程根 `.gitignore`。
    pending_reload: HashMap<PathBuf, bool>,
    watcher: Arc<FsWatcher>,
    watched: HashSet<PathBuf>,
    /// 当前挂着的项目;换项目时整表作废。
    current_project: Option<String>,
    /// 项目 + 根路径 + 后端/连接配置的身份签名。连接配置原地修改时也会变化。
    source_signature: Option<String>,
    /// 每次 source signature 变化递增，后台结果回写前必须对号。
    source_generation: u64,
    /// 应用内文件复制剪贴板；显式绑定项目和后端，不与系统文本剪贴板混用。
    file_clipboard: Option<FileClipboardEntry>,
    /// 当前 FileTree 同时只允许一个 mutation/transfer。
    operation_busy: bool,
    operation_label: Option<String>,
    /// 在途操作开始时的来源身份。切项目不会清空它；任务结束时只允许原操作
    /// 释放全局闸，并仅在项目/根/后端仍一致时刷新视图。
    active_operation_context: Option<FileOperationContext>,
    active_operation_suppressed_path: Option<PathBuf>,
    /// 正在被删除、重命名或批量改写的子树。操作结束前禁止 watcher / 展开态补列
    /// 重新挂载它，避免大目录删除时 watcher 事件洪泛和半成品缓存回写。
    suppressed_subtrees: HashSet<PathBuf>,
    /// 外部文件拖放当前命中的远程目标目录。
    external_drop_target: Option<ExternalDropTarget>,
    /// git 状态:相对项目根的 `/` 分隔路径 → 状态字母(M/A/D/R/?/C)。
    git_status: HashMap<String, String>,
    /// 排着的 git 状态刷新(去抖到点时刻);`None` = 没排。
    git_refresh_at: Option<Instant>,
    /// 压缩链的每一段 → 「产出这条链的那次列目录」。中段变化要重列它、重新压缩。
    chain_owner: HashMap<PathBuf, PathBuf>,
    /// 根目录还在列、且一份内容都还没有 —— 三态占位里的 loading 那一档。
    root_loading: bool,
    /// 上一次列根目录的错误原文。
    root_error: Option<String>,
    /// 当前项目是「断链的 SSH 远程项目」——连接被删,什么都列不出来。
    /// 与 `root_error` 分开存是因为它**不是一次加载失败**,而是一个静态状态:
    /// 不发请求、不重试,直接画那句提示。
    remote_broken: bool,
    /// 每行一个焦点句柄(原版每行 `tabIndex={0}`)。行拿到焦点后 Enter/Space
    /// 与 ←→ 才有落点,见 [`Self::on_row_key`]。
    row_focus: HashMap<PathBuf, gpui::FocusHandle>,
    _fs_task: Task<()>,
    _git_task: Task<()>,
}

impl FileTree {
    fn note_external_drop_target(
        &mut self,
        hit_id: &str,
        target: &Path,
        event: &DragMoveEvent<ExternalPaths>,
        cx: &mut Context<Self>,
    ) {
        if event.bounds.contains(&event.event.position) {
            if self
                .external_drop_target
                .as_ref()
                .is_none_or(|active| active.hit_id != hit_id || active.target_dir != target)
            {
                self.external_drop_target = Some(ExternalDropTarget {
                    hit_id: hit_id.to_string(),
                    target_dir: target.to_path_buf(),
                });
                cx.notify();
            }
        } else if self
            .external_drop_target
            .as_ref()
            .is_some_and(|active| active.hit_id == hit_id)
        {
            self.external_drop_target = None;
            cx.notify();
        }
    }

    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this: &mut Self, _, cx| {
            this.sync_project(cx);
            cx.notify();
        })
        .detach();

        // 丢过去的是**变动文件的完整路径**:重列只要它的父目录,但技术栈缓存的
        // 失效判据要看文件名本身(`Cargo.toml` / `package.json` 之类)
        let (tx, mut rx) = mpsc::unbounded::<PathBuf>();
        let watcher = Arc::new(FsWatcher::new(move |change| {
            // notify 自己的线程:只把「什么变了」丢过去,重列在主线程排。
            let _ = tx.unbounded_send(change.path);
        }));

        let fs_task = cx.spawn(async move |this, cx| {
            while let Some(path) = rx.next().await {
                let dir = match path.parent() {
                    Some(parent) => parent.to_path_buf(),
                    None => path.clone(),
                };
                if this
                    .update(cx, |tree: &mut FileTree, cx| {
                        if tree.path_is_suppressed(&path) {
                            return;
                        }
                        tree.invalidate(&dir, cx);
                        // 原版第二条:`fs-change` 且属于当前项目 → 500ms 去抖刷 git 状态。
                        // watcher 是按项目根注册的,能走到这儿的必然属于当前项目。
                        tree.schedule_git_refresh();
                        tree.invalidate_dir_kind(&path, &dir, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        // 100ms 节拍:收 git 输出旁路的命中 + 到点跑去抖的那次刷新。
        // 与 Git 面板同一条旁路、同一个节拍常数,只是各自一个游标(见 git_watch)。
        let git_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(git_watch::POLL_MS))
                    .await;
                if this
                    .update(cx, |tree: &mut FileTree, cx| tree.tick_git(cx))
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
            dir_request_ids: HashMap::new(),
            next_dir_request_id: 0,
            pending_reload: HashMap::new(),
            watcher,
            watched: HashSet::new(),
            current_project: None,
            source_signature: None,
            source_generation: 0,
            file_clipboard: None,
            operation_busy: false,
            operation_label: None,
            active_operation_context: None,
            active_operation_suppressed_path: None,
            suppressed_subtrees: HashSet::new(),
            external_drop_target: None,
            git_status: HashMap::new(),
            git_refresh_at: None,
            chain_owner: HashMap::new(),
            root_loading: false,
            root_error: None,
            remote_broken: false,
            row_focus: HashMap::new(),
            _fs_task: fs_task,
            _git_task: git_task,
        };
        this.sync_project(cx);
        this
    }

    /// 排一次去抖刷新(原版 `debouncedRefresh`,500ms)。重复排只推后到点时刻。
    fn schedule_git_refresh(&mut self) {
        self.git_refresh_at = Some(Instant::now() + Duration::from_millis(git_watch::DEBOUNCE_MS));
    }

    /// 节拍:旁路命中就排一次去抖,到点就真去拉。
    fn tick_git(&mut self, cx: &mut Context<Self>) {
        if git_watch::drain_hit_for(git_watch::Subscriber::FileTree) {
            self.schedule_git_refresh();
        }
        if self.git_refresh_at.is_some_and(|at| Instant::now() >= at) {
            self.git_refresh_at = None;
            self.load_git_status(cx);
        }
    }

    /// 拉一次 git 状态。`get_git_status` 要跑 libgit2 的全量 status,**必须丢后台**。
    ///
    /// 失败一律清空(原版 `.catch(() => setGitStatusMap(new Map()))`):
    /// 不是 git 仓库 / 仓库坏了的时候,留着上一个项目的状态字母比没有更糟。
    fn load_git_status(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.operation_context(cx) else {
            self.git_status.clear();
            return;
        };
        if !matches!(&context.backend, FileBackendIdentity::Local) {
            self.git_status.clear();
            return;
        }
        let root = context.root.clone();
        cx.spawn(async move |this, cx| {
            let probe = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_git_status(&probe) })
                .await;
            let _ = this.update(cx, |tree: &mut FileTree, cx| {
                // 回来时项目、连接或 source generation 都可能已经变化；完整上下文
                // 不相等就丢弃，避免同路径项目之间串入迟到的 git 状态。
                if tree.operation_context(cx).as_ref() != Some(&context) {
                    return;
                }
                tree.git_status = result
                    .map(|files| {
                        files
                            .into_iter()
                            .map(|f| (f.path.replace('\\', "/"), f.status_label))
                            .collect()
                    })
                    .unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
    }

    /// 活动项目变了:清空缓存与监听,重列根目录。
    fn sync_project(&mut self, cx: &mut Context<Self>) {
        let (project_id, root, remote, broken, signature) = {
            let store = self.store.read(cx);
            match store.active_project() {
                Some(p) => {
                    let is_remote = store.is_remote_project(&p.id);
                    let conn = store.remote_connection_of(&p.id);
                    let signature = match &conn {
                        Some(conn) => format!(
                            "{}|{}|ssh:{:016x}",
                            p.id,
                            p.path,
                            crate::remote_ssh::connection_fingerprint(conn)
                        ),
                        None if is_remote => format!("{}|{}|ssh:broken", p.id, p.path),
                        None => format!("{}|{}|local", p.id, p.path),
                    };
                    (
                        Some(p.id.clone()),
                        Some(PathBuf::from(&p.path)),
                        conn.is_some(),
                        // 断链 = 是远程项目但连接查不到
                        is_remote && conn.is_none(),
                        Some(signature),
                    )
                }
                None => (None, None, false, false, None),
            }
        };
        if signature == self.source_signature {
            return;
        }
        for dir in std::mem::take(&mut self.watched) {
            self.watcher.unwatch(&dir);
        }
        self.entries.clear();
        self.loading.clear();
        self.dir_request_ids.clear();
        self.pending_reload.clear();
        self.chain_owner.clear();
        self.git_status.clear();
        self.git_refresh_at = None;
        self.root_error = None;
        self.current_project = project_id;
        self.source_signature = signature;
        self.source_generation = self.source_generation.wrapping_add(1);
        self.suppressed_subtrees.clear();
        if self.operation_busy
            && let (Some(active), Some(current), Some(path)) = (
                self.active_operation_context.as_ref(),
                self.operation_context(cx),
                self.active_operation_suppressed_path.as_ref(),
            )
            && same_file_source(active, &current)
        {
            self.suppressed_subtrees.insert(path.clone());
        }
        self.external_drop_target = None;
        self.remote_broken = broken;
        self.git_status.clear();
        // 没有项目 / 远程项目都把旁路那一份关掉:远程不拉 git 状态,
        // reader 线程上那道总闸能少开一个人是一个
        git_watch::set_enabled_for(
            git_watch::Subscriber::FileTree,
            root.is_some() && !remote && !broken,
        );
        if broken {
            // 断链:不发任何请求,直接给那句明确提示(项目仍可见、可删)
            self.root_loading = false;
            self.root_error = Some(t("fileTree", "remote.broken").to_string());
            return;
        }
        if let Some(root) = root {
            self.root_loading = true;
            self.load_dir(root.clone(), root, cx);
            // 原版第一条:切项目时与 `list_directory` 并发拉一次。
            // 远程项目跳过(远程 Git 二期)
            if !remote {
                self.load_git_status(cx);
            }
        } else {
            self.root_loading = false;
        }
    }

    /// 当前项目的远程连接(`None` = 本地项目 **或** 断链)。
    ///
    /// 返回克隆:它要被丢进 background executor(`remote_ssh` 的入口全是阻塞函数)。
    fn remote_conn(&self, cx: &App) -> Option<mt_config::SshConnection> {
        let store = self.store.read(cx);
        let id = store.active_project_id.as_deref()?;
        store.remote_connection_of(id)
    }

    fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .active_project()
            .map(|p| PathBuf::from(&p.path))
    }

    fn operation_context(&self, cx: &App) -> Option<FileOperationContext> {
        let store = self.store.read(cx);
        let project = store.active_project()?;
        let backend = if store.is_remote_project(&project.id) {
            match store.remote_connection_of(&project.id) {
                Some(connection) => FileBackendIdentity::Remote {
                    connection_id: connection.id.clone(),
                    connection_fingerprint: crate::remote_ssh::connection_fingerprint(&connection),
                },
                None => FileBackendIdentity::BrokenRemote,
            }
        } else {
            FileBackendIdentity::Local
        };
        Some(FileOperationContext {
            project_id: project.id.clone(),
            root: PathBuf::from(&project.path),
            backend,
            generation: self.source_generation,
        })
    }

    fn path_is_suppressed(&self, path: &Path) -> bool {
        self.suppressed_subtrees
            .iter()
            .any(|suppressed| path.starts_with(suppressed))
    }

    /// 列一个目录(后台线程)+ 挂监听。
    ///
    /// `refresh_ignore` **只对远程有效**:强制后端重读远程根 `.gitignore`
    /// (头部刷新按钮那一路,原版 `loadRootEntries(true)`)。
    fn load_dir(&mut self, root: PathBuf, dir: PathBuf, cx: &mut Context<Self>) {
        self.load_dir_with(root, dir, false, false, cx);
    }

    fn load_dir_with(
        &mut self,
        root: PathBuf,
        dir: PathBuf,
        refresh_ignore: bool,
        queue_if_loading: bool,
        cx: &mut Context<Self>,
    ) {
        if self.path_is_suppressed(&dir) {
            return;
        }
        if self.loading.contains(&dir) {
            if queue_if_loading {
                self.pending_reload
                    .entry(dir)
                    .and_modify(|pending| *pending |= refresh_ignore)
                    .or_insert(refresh_ignore);
            }
            return;
        }
        self.loading.insert(dir.clone());
        self.next_dir_request_id = self.next_dir_request_id.wrapping_add(1);
        let request_id = self.next_dir_request_id;
        self.dir_request_ids.insert(dir.clone(), request_id);

        // 远程项目**不注册 watcher**:远端文件系统本机监听不到
        let remote = self.remote_conn(cx);
        if remote.is_none()
            && self.watched.insert(dir.clone())
            && let Err(err) = self.watcher.watch(&dir, root.to_string_lossy().as_ref())
        {
            eprintln!("[files] 监听 {} 失败: {err:#}", dir.display());
        }

        let task_dir = dir.clone();
        let task_root = root.clone();
        let request_project = self.current_project.clone();
        let request_signature = self.source_signature.clone();
        let request_generation = self.source_generation;
        // 根目录那一趟额外承担三态占位(loading / 加载失败 / 刷新失败)
        let is_root = dir == root;
        cx.spawn(async move |this, cx| {
            // 两条路都是阻塞 IO(本地要逐级读 .gitignore,远程是 SFTP 往返),
            // 必须离开主线程;单链压缩最多再串行列 7 层,**整段都在后台**跑完再回来。
            // 远程**不压缩单链**:逐级 SFTP 往返太贵(原版原话)
            let result = cx
                .background_executor()
                .spawn(async move {
                    let entries = crate::remote_ssh::list_directory_for(
                        remote.as_ref(),
                        &task_root,
                        &task_dir,
                        refresh_ignore,
                    )
                    .map_err(|e| anyhow::anyhow!(e))?;
                    if remote.is_some() {
                        return anyhow::Ok(entries.into_iter().map(|e| (e, Vec::new())).collect());
                    }
                    let chains = compact_dir_chains(entries, |d| {
                        mt_project::fs::list_directory(&task_root, d).unwrap_or_default()
                    });
                    anyhow::Ok(chains)
                })
                .await;
            let _ = this.update(cx, |tree: &mut FileTree, cx| {
                if tree.current_project != request_project
                    || tree.source_signature != request_signature
                    || tree.source_generation != request_generation
                    || tree.dir_request_ids.get(&dir) != Some(&request_id)
                {
                    return;
                }
                tree.loading.remove(&dir);
                tree.dir_request_ids.remove(&dir);
                if let Some(pending_refresh_ignore) = tree.pending_reload.remove(&dir) {
                    tree.load_dir_with(
                        root.clone(),
                        dir.clone(),
                        pending_refresh_ignore,
                        false,
                        cx,
                    );
                    return;
                }
                if is_root {
                    tree.root_loading = false;
                }
                match result {
                    Ok(rows) => {
                        if is_root {
                            tree.root_error = None;
                        }
                        // 这一趟列出来的压缩链先整份作废,再按新结果登记 ——
                        // 链缩短/消失时旧的中段登记不能留着
                        tree.chain_owner.retain(|_, owner| owner != &dir);
                        let mut entries = Vec::with_capacity(rows.len());
                        for (entry, chain) in rows {
                            if chain.len() > 1 {
                                for segment in &chain {
                                    tree.chain_owner.insert(segment.clone(), dir.clone());
                                    // 链上**每一段**都要监听:后端 watcher 是
                                    // NonRecursive,中段新增文件否则无人上报,
                                    // 压缩前提破了也不知道
                                    if tree.watched.insert(segment.clone())
                                        && let Err(err) = tree
                                            .watcher
                                            .watch(segment, root.to_string_lossy().as_ref())
                                    {
                                        eprintln!(
                                            "[files] 监听 {} 失败: {err:#}",
                                            segment.display()
                                        );
                                    }
                                }
                            }
                            entries.push(entry);
                        }
                        // 根目录一级子目录的子工程探测(原版 `FileTree.tsx:488-491`
                        // 那个 effect):不必展开就能在树里看到技术栈图标。
                        // 忽略项不探 —— 图标那一路也只在 `!entry.ignored` 时才换。
                        // **远程项目跳过**:`ensure_dir_kinds` 是本机 `stat`,
                        // 拿远端 POSIX 路径去探等于探一个不存在的本机目录
                        if is_root && !tree.is_remote(cx) {
                            let probe: Vec<String> = entries
                                .iter()
                                .filter(|e| e.is_dir && !e.ignored)
                                .map(|e| e.path.to_string_lossy().to_string())
                                .collect();
                            tree.store
                                .update(cx, |store, cx| store.ensure_dir_kinds(probe, cx));
                        }
                        tree.entries.insert(dir, entries);
                    }
                    Err(err) => {
                        eprintln!("[files] 列目录失败: {err:#}");
                        if is_root {
                            tree.root_error = Some(format!("{err:#}"));
                        } else {
                            // 子目录列失败也要在 `entries` 里留下这一条(空的)——
                            // 展开态补列(`missing_expanded_dirs`)的判据就是
                            // 「`entries` 里有没有这一项」,不落这条的话
                            // render → 补列 → 失败 → notify → render 会绕成死循环。
                            // `or_default` 不动已有内容:刷新失败时旧内容照旧留着,
                            // 与根目录那条「有旧内容就静默保留」同一口径
                            tree.entries.entry(dir).or_default();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 头部刷新按钮 / 加载失败后的「重试」:重列根目录 + 重拉 git 状态
    /// (原版 `loadRootEntries() + loadGitStatus()` 那一对)。
    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        // 断链:没什么可刷的(连接都没了),提示留着
        if self.remote_broken {
            return;
        }
        let Some(root) = self.project_root(cx) else {
            return;
        };
        // 没有内容可显示时才亮 loading —— 有旧内容就静默重列(原版同一条口径)
        if !self.entries.contains_key(&root) {
            self.root_loading = true;
        }
        let remote = self.is_remote(cx);
        // 手动刷新时强制重读远程根 `.gitignore`(原版 `loadRootEntries(true)`);
        // 本地那一路后端不认这个参数,传什么都一样
        self.load_dir_with(root.clone(), root, remote, true, cx);
        if !remote {
            self.load_git_status(cx);
        }
        cx.notify();
    }

    /// 当前项目是 SSH 远程项目吗(**断链也算** —— 那仍是个远程项目)。
    fn is_remote(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        store
            .active_project_id
            .as_deref()
            .is_some_and(|id| store.is_remote_project(id))
    }

    /// 目录内容变了:已列过的重列一次。
    ///
    /// 压缩链的**任何一段**变了也算 —— 重列产出这条链的那次列目录,让它按
    /// 新内容重新压缩(原版 `midChainHit` 那段的等价物,这里连链尾一起管:
    /// 链尾多出一个子目录同样能把链接长,重列一次比漏一次划算)。
    fn invalidate(&mut self, dir: &Path, cx: &mut Context<Self>) {
        let target = if self.entries.contains_key(dir) {
            dir.to_path_buf()
        } else if let Some(owner) = self.chain_owner.get(dir) {
            owner.clone()
        } else {
            return;
        };
        let Some(root) = self.project_root(cx) else {
            return;
        };
        self.load_dir_with(root, target, false, true, cx);
    }

    /// 技术栈缓存的失效(`useProjectKinds.ts:88-103` 的 `fs-change` 监听)。
    ///
    /// 判据逐条照抄:变动的**文件名**在标记文件表里,且它的**父目录正好是某个
    /// 本地项目的根**。原版注释点明了为什么只认项目根 —— 只有活跃项目的根目录
    /// 被 watch,那正是唯一能在应用内改到这些文件的场景。
    fn invalidate_dir_kind(&mut self, path: &Path, dir: &Path, cx: &mut Context<Self>) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        if !crate::project_kind::is_marker_file(name) {
            return;
        }
        let parent = crate::project_kind::norm_path(&dir.to_string_lossy());
        let target = self
            .store
            .read(cx)
            .projects()
            .iter()
            .find(|p| {
                p.ssh_connection_id.is_none() && crate::project_kind::norm_path(&p.path) == parent
            })
            .map(|p| p.path.clone());
        if let Some(target) = target {
            self.store
                .update(cx, |store, cx| store.remove_dir_kind(&target, cx));
        }
    }

    // ─── 键盘导航(`FileTree.tsx:197-209`) ─────────────────────

    /// 行的焦点句柄(按需建、跨帧稳定)。
    fn row_focus(&mut self, path: &Path, cx: &mut Context<Self>) -> gpui::FocusHandle {
        self.row_focus
            .entry(path.to_path_buf())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// 行按键。逐条照抄原版:目录 Enter/Space/→ 展开、← 折叠;文件 Enter/Space 开预览。
    ///
    /// ⚠️ **→ 只在折叠时生效、← 只在展开时生效**(原版那两个 `&& !expanded` /
    /// `&& expanded`),否则方向键会变成 toggle,在展开的目录上按 → 反而折叠。
    fn on_row_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        path: &Path,
        is_dir: bool,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "enter" | "space" => {
                cx.stop_propagation();
                if is_dir {
                    self.toggle_dir(path.to_path_buf(), cx);
                } else {
                    self.open_file(path.to_path_buf(), window, cx);
                }
            }
            "right" if is_dir && !expanded => {
                cx.stop_propagation();
                self.toggle_dir(path.to_path_buf(), cx);
            }
            "left" if is_dir && expanded => {
                cx.stop_propagation();
                self.toggle_dir(path.to_path_buf(), cx);
            }
            _ => {}
        }
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
        } else if self.chain_owner.contains_key(&path) {
            // 压缩链上的目录**折叠了也要继续监听**:链的成立与否只看内容,
            // 不看展开状态(原版 `watchActive = expanded || chainPaths !== undefined`)
        } else {
            self.watched.remove(&path);
            self.watcher.unwatch(&path);
        }
        cx.notify();
    }

    /// 单击文件行 = 开文件预览器(`FileTree.tsx:151-155` 的 `handleToggle`:
    /// `!entry.isDir → onViewFile(entry.path)`)。
    ///
    /// **原版没有「双击调外部编辑器」这条路** —— 文件行上只有预览一条,
    /// 外部编辑器在原版只出现在项目级(头部按钮)与右键「使用默认工具打开」。
    /// AA 批之前 GPUI 侧的双击调编辑器是预览器缺位时的临时替身,现在撤掉:
    /// 留着的话双击会先开预览器再拉起编辑器(gpui 的双击是两个 click 事件,
    /// click_count 依次为 1、2),两个窗口一起冒出来。
    fn open_file(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        crate::workbench_area::open_active_file(self.store.clone(), path, None, window, cx);
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
            self.load_dir_with(root, dir, false, true, cx);
        }
        cx.notify();
    }

    /// 删除前解除目标子树 watcher 并清掉缓存，避免大目录逐文件事件洪泛；远程树虽
    /// 没 watcher，也共用缓存清理。失败后父目录重列会按展开状态逐层恢复。
    fn detach_subtree(&mut self, target: &Path) {
        let watched: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|path| path.starts_with(target))
            .cloned()
            .collect();
        for path in watched {
            self.watched.remove(&path);
            self.watcher.unwatch(&path);
        }
        self.entries.retain(|path, _| !path.starts_with(target));
        self.loading.retain(|path| !path.starts_with(target));
        self.dir_request_ids
            .retain(|path, _| !path.starts_with(target));
        self.pending_reload
            .retain(|path, _| !path.starts_with(target));
        self.chain_owner
            .retain(|path, owner| !path.starts_with(target) && !owner.starts_with(target));
        self.row_focus.retain(|path, _| !path.starts_with(target));
    }

    /// 把树按展开状态拍平成可渲染的行。
    #[allow(clippy::too_many_arguments)]
    fn rows(
        &self,
        project_id: &str,
        root: &Path,
        dir: &Path,
        depth: usize,
        cx: &App,
        out: &mut Vec<Row>,
    ) {
        let Some(entries) = self.entries.get(dir) else {
            return;
        };
        let store = self.store.read(cx);
        let root_str = root.to_string_lossy().to_string();
        for entry in entries {
            let key = entry.path.to_string_lossy().to_string();
            let expanded = entry.is_dir && store.is_dir_expanded(project_id, &key);
            // git 状态表的键是 `/` 分隔的相对路径,与 `getRelativePath().replace(/\\/g,'/')` 同构
            let rel = fs_ops::relative_path(&key, &root_str).replace('\\', "/");
            let git = match self.git_status.get(&rel) {
                Some(label) => Some((label.clone(), false)),
                // 目录自身没有状态时才汇总子树(原版就是这个 if/else 的顺序)
                None if entry.is_dir => {
                    rollup_dir_label(&self.git_status, &rel).map(|l| (l.to_string(), true))
                }
                None => None,
            };
            out.push(Row {
                name: entry.name.clone(),
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                ignored: entry.ignored,
                depth,
                expanded,
                rel,
                git,
                // 一级子目录被识别为子工程时领位换技术栈徽标(`FileTree.tsx:346-351`)。
                // 条件一字不差:目录、depth == 0、非远程、未被 gitignore
                kind: (entry.is_dir && depth == 0 && !entry.ignored)
                    .then(|| store.dir_kind(&key))
                    .flatten()
                    .flatten(),
            });
            if expanded {
                self.rows(project_id, root, &entry.path, depth + 1, cx, out);
            }
        }
    }
}

fn same_file_source(left: &FileOperationContext, right: &FileOperationContext) -> bool {
    left.project_id == right.project_id && left.root == right.root && left.backend == right.backend
}

/// 「展开着、却一份内容都没列过」的目录 —— 要补列的那些。
///
/// 展开状态存在 [`AppStore`] 里并**落盘**(`ProjectConfig::expanded_dirs`),
/// 而 [`FileTree::entries`] 是纯内存缓存,[`FileTree::sync_project`] 换项目时整表清掉、
/// 只重列根目录(面板重建、冷启动同理)。两者一留一清,回到该项目时目录行还是
/// 展开态(`▾`),但 [`FileTree::rows`] 在 `entries` 里查不到内容 → 那一层一行不画,
/// 就成了「展开着,里头空的」。补列把两边接回去。
///
/// 只顺着**祖先全已列出**的那条链往下走:陈旧的深层展开记录(祖先早折叠了的)
/// 翻不到,不会白列一趟 —— 远程项目一次列目录是一趟 SFTP 往返,这笔不是小钱。
/// 一轮只补一层,列回来 notify 触发下一帧再补下一层,逐层收敛。
fn missing_expanded_dirs(
    entries: &HashMap<PathBuf, Vec<FileEntry>>,
    dir: &Path,
    is_expanded: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) {
    let Some(rows) = entries.get(dir) else {
        return;
    };
    for entry in rows {
        if !entry.is_dir || !is_expanded(&entry.path) {
            continue;
        }
        if entries.contains_key(&entry.path) {
            missing_expanded_dirs(entries, &entry.path, is_expanded, out);
        } else {
            out.push(entry.path.clone());
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
    /// 相对项目根、`/` 分隔的路径(git 状态表的键,「查看变更」也拿它当参数)。
    rel: String,
    /// git 状态字母 + **是不是汇总来的**(汇总的那枚画淡一档)。
    git: Option<(String, bool)>,
    /// 一级子目录的技术栈徽标(`None` = 用普通文件夹图标)。
    kind: Option<mt_ui::icons::ProjectKind>,
}

// ─── 右键菜单 ─────────────────────────────────────────────────

/// 文件树右键菜单的**项序**。`None` = 分隔线。
///
/// 逐条对照 `FileTree.tsx:210-325`。「查看变更」(`ViewDiff`)在 V 批把
/// [`crate::git_diff::open_file_diff`] 建好之后补上,**条件与原版一字不差**:
/// 非目录、且这个文件在 git 状态表里有条目,前置一条分隔线接在最末。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileMenuAction {
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

fn file_menu_actions(
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

// ─── 单链目录压缩(`FileTree.tsx:50-86` 的 `compactDirChains`) ──

/// 链深上限。每深一层多一次**串行** `list_directory`(后端还要跑 gitignore 匹配),
/// 8 层足够覆盖 Java 式深包名(`src/main/java/com/foo/bar`)。
const MAX_CHAIN: usize = 8;

/// IDE 的 "compact middle packages":目录**一路只有唯一子目录、没有文件**时,
/// 折成一行 `main/java/com/…`。
///
/// `list` 是「列一个目录」的闭包(真跑时是阻塞的 `list_directory`,单测里喂假表),
/// 返回值与入参一一对应:`(展示用的条目, 链上每一段的路径)`。
/// **没压缩的条目 `chain` 长度为 1**,调用方据此判断要不要登记链。
///
/// 三条规则照抄原版:非目录 / 被 gitignore 的条目不参与;继续的条件是
/// 「唯一子项且它是未被忽略的目录」;拼名字用 `/` 而**不是**平台分隔符。
fn compact_dir_chains(
    entries: Vec<FileEntry>,
    mut list: impl FnMut(&Path) -> Vec<FileEntry>,
) -> Vec<(FileEntry, Vec<PathBuf>)> {
    entries
        .into_iter()
        .map(|mut entry| {
            if !entry.is_dir || entry.ignored {
                let chain = vec![entry.path.clone()];
                return (entry, chain);
            }
            let mut chain = vec![entry.path.clone()];
            let mut name = entry.name.clone();
            while chain.len() < MAX_CHAIN {
                let kids = list(chain.last().expect("链至少有一段"));
                let [only] = kids.as_slice() else {
                    break;
                };
                if !only.is_dir || only.ignored {
                    break;
                }
                name.push('/');
                name.push_str(&only.name);
                chain.push(only.path.clone());
            }
            if chain.len() > 1 {
                entry.name = name;
                // 展示的是链尾那个**真实**目录:展开它列的就是链尾的子项
                entry.path = chain.last().cloned().expect("链至少有一段");
            }
            (entry, chain)
        })
        .collect()
}

// ─── git 状态着色(`FileTree.tsx:359-400`) ───────────────────

/// 状态字母 → 颜色。认不出的字母退 `--text-muted`(原版的 `?? text-muted`)。
fn git_color(label: &str) -> Hsla {
    match label {
        "M" => ui::color_warning(),
        "A" | "?" => ui::color_success(),
        "D" | "C" => ui::color_error(),
        "R" => ui::color_info(),
        _ => ui::text_muted(),
    }
}

/// 目录汇总的优先级(原版 `PRIORITY`,数越大越优先)。0 = 不参与汇总。
fn git_priority(label: &str) -> u8 {
    match label {
        "C" => 6,
        "D" => 5,
        "M" => 4,
        "A" => 3,
        "R" => 2,
        "?" => 1,
        _ => 0,
    }
}

/// 目录行的汇总字母:扫状态表里所有以 `rel/` 开头的条目,取优先级最高的那个。
///
/// `rel` 传空串(理论上的项目根)时前缀是 `"/"`,与原版一样谁也匹配不上 ——
/// 根不是树里的一行,不会真的走到这一支。
fn rollup_dir_label<'a>(status: &'a HashMap<String, String>, rel: &str) -> Option<&'a str> {
    let prefix = if rel.ends_with('/') {
        rel.to_string()
    } else {
        format!("{rel}/")
    };
    let mut best: Option<(&str, u8)> = None;
    for (path, label) in status {
        if !path.starts_with(&prefix) {
            continue;
        }
        let p = git_priority(label);
        if p > best.map(|(_, bp)| bp).unwrap_or(0) {
            best = Some((label.as_str(), p));
        }
    }
    best.map(|(label, _)| label)
}

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
fn spawn_tree_op(
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

fn copy_to_file_clipboard(
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

fn paste_file_clipboard(
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

fn open_entry_in_terminal(
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

fn start_upload(
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

fn choose_upload_paths(
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

fn start_download(
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

/// 一行(文件/目录)的右键菜单。
fn file_menu(
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

fn background_menu(
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

/// 「新建文件 / 新建文件夹」:问名字 → 建 → 展开父目录并重列。
fn new_entry_prompt(
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

/// 快捷键提示里的修饰键名(与 `search_modal` 那份同规则)。
fn mod_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeaderActionCapabilities {
    show_upload: bool,
    mutations_enabled: bool,
    paste_enabled: bool,
}

fn header_action_capabilities(
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

/// 头部 26×26 图标钮共用的外观(`FileTree.tsx:734`)。
fn header_button(id: &'static str) -> gpui::Stateful<gpui::Div> {
    header_action_button(id, true)
}

fn header_action_button(id: &'static str, enabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(26.0))
        .h(px(26.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        .text_color(ui::text_muted())
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|el| el.text_color(ui::text_primary()).bg(ui::border_subtle()))
        })
        .when(!enabled, |el| el.opacity(0.38))
}

/// 放大镜。原版是 `viewBox="0 0 16 16"` 的 `circle(7,7,r=4.2)` + `M10.2 10.2L14 14`
/// (`FileTree.tsx:736-739`),这里按 VectorIcon 的单位方框除以 16;
/// 线宽同样是比例:`1.4 / 16 = 0.0875`。
const SEARCH_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.0875,
        Geom::Circle {
            c: (0.4375, 0.4375),
            r: 0.2625,
        },
    ),
    Shape::line(
        Ink::Current,
        0.0875,
        Geom::Polyline(&[(0.6375, 0.6375), (0.875, 0.875)]),
    ),
];

/// 刷新:`M13.5 8a5.5 5.5 0 1 1-1.7-3.97`(圆心 (8,8) 半径 5.5,从 3 点钟顺时针
/// 扫到 -46.2°,即 313.8°)+ 右上角那个箭头钩 `M13.6 2.6v3.2h-3.2`。
const REFRESH_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.0875,
        Geom::Arc {
            c: (0.5, 0.5),
            r: 0.34375,
            from: 0.0,
            sweep: 313.8,
        },
    ),
    Shape::line(
        Ink::Current,
        0.0875,
        Geom::Polyline(&[(0.85, 0.1625), (0.85, 0.3625), (0.65, 0.3625)]),
    ),
];

const FILE_SHAPES: &[Shape] = &[Shape::line(
    Ink::Current,
    0.075,
    Geom::Polyline(&[
        (0.22, 0.08),
        (0.62, 0.08),
        (0.82, 0.28),
        (0.82, 0.92),
        (0.22, 0.92),
        (0.22, 0.08),
        (0.62, 0.08),
        (0.62, 0.28),
        (0.82, 0.28),
    ]),
)];

const FOLDER_SHAPES: &[Shape] = &[Shape::line(
    Ink::Current,
    0.075,
    Geom::Polyline(&[
        (0.08, 0.24),
        (0.38, 0.24),
        (0.48, 0.36),
        (0.92, 0.36),
        (0.92, 0.86),
        (0.08, 0.86),
        (0.08, 0.24),
    ]),
)];

const UPLOAD_MARK_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.095,
        Geom::Polyline(&[(0.5, 0.76), (0.5, 0.42)]),
    ),
    Shape::line(
        Ink::Current,
        0.095,
        Geom::Polyline(&[(0.34, 0.56), (0.5, 0.40), (0.66, 0.56)]),
    ),
];

const PLUS_MARK_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.095,
        Geom::Polyline(&[(0.66, 0.68), (0.92, 0.68)]),
    ),
    Shape::line(
        Ink::Current,
        0.095,
        Geom::Polyline(&[(0.79, 0.55), (0.79, 0.81)]),
    ),
];

const PASTE_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.075,
        Geom::Polyline(&[
            (0.28, 0.22),
            (0.18, 0.22),
            (0.18, 0.92),
            (0.78, 0.92),
            (0.78, 0.82),
        ]),
    ),
    Shape::line(
        Ink::Current,
        0.075,
        Geom::Polyline(&[
            (0.38, 0.12),
            (0.70, 0.12),
            (0.70, 0.30),
            (0.38, 0.30),
            (0.38, 0.12),
        ]),
    ),
    Shape::line(
        Ink::Current,
        0.075,
        Geom::Polyline(&[
            (0.34, 0.30),
            (0.30, 0.30),
            (0.30, 0.78),
            (0.86, 0.78),
            (0.86, 0.30),
            (0.74, 0.30),
        ]),
    ),
];

/// 编辑器选择器的下拉箭头(原版 8×8 的 `M1.5 3L4 5.5L6.5 3`)。
const CARET_SHAPES: &[Shape] = &[Shape::line(
    Ink::Current,
    0.15,
    Geom::Polyline(&[(0.1875, 0.375), (0.5, 0.6875), (0.8125, 0.375)]),
)];

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.external_drop_target = None;
        }
        let project_name = self.store.read(cx).active_project().map(|p| p.name.clone());
        let editors: Vec<String> = self
            .store
            .read(cx)
            .config()
            .editors
            .iter()
            .map(|e| e.name.clone())
            .collect();
        let default_editor = self
            .store
            .read(cx)
            .config()
            .default_editor
            .clone()
            .filter(|name| editors.iter().any(|e| e == name))
            .or_else(|| editors.first().cloned());
        let header_capabilities = header_action_capabilities(
            self.operation_context(cx).as_ref(),
            self.operation_busy,
            self.file_clipboard.as_ref(),
        );

        let is_remote = self.is_remote(cx);
        let mut header = div()
            .id("file-tree-header")
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .flex_none()
            // 180px 窄栏装不下远程侧 6 个固定 26px 图标。保持按钮尺寸并允许
            // 整条头部横向滚动，所有动作仍可达；宽栏内容未溢出时行为不变。
            .overflow_x_scroll()
            .px(px(10.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .flex_1()
                    .min_w(px(60.0))
                    .truncate()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    // 有项目时带项目名(`panels.filesOf`),没有就退回纯「文件」
                    .child(match &project_name {
                        Some(name) => tr!("panels", "filesOf", project = name.clone()),
                        None => t("panels", "files").to_string(),
                    }),
            );

        if project_name.is_some() {
            let store_for_search = self.store.clone();
            header = header.child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .gap(px(4.0))
                    .when(!is_remote, |el| {
                        el.child(
                            // 搜索 = 全局 SearchModal(不是文件名过滤),与 Ctrl+Shift+F 同一个入口
                            header_button("file-tree-search")
                                .tooltip(|window, cx| {
                                    // `{mod}` 插值不能走 `tr!`(参数位是 `$name:ident`,
                                    // `mod` 是 Rust 关键字塞不进去)—— 与 search_modal 同一个坑
                                    Tooltip::new(mt_i18n::t_args(
                                        "fileTree",
                                        "header.searchTitle",
                                        &[("mod", mod_label())],
                                    ))
                                    .build(window, cx)
                                })
                                .on_click(move |_event, window, cx| {
                                    crate::search_modal::open(store_for_search.clone(), window, cx);
                                })
                                .child(
                                    VectorIcon::new(SEARCH_SHAPES, px(13.0)).ink(ui::text_muted()),
                                ),
                        )
                    })
                    .child(
                        header_button("file-tree-refresh")
                            // 远程项目多一句:刷新会重读远程根 `.gitignore`
                            // (原版 `FileTree.tsx` 的 `remote.refreshTitle`)
                            .tooltip({
                                let remote = is_remote;
                                move |window, cx| {
                                    Tooltip::new(if remote {
                                        t("fileTree", "remote.refreshTitle")
                                    } else {
                                        t("fileTree", "header.refresh")
                                    })
                                    .build(window, cx)
                                }
                            })
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.refresh_root(cx);
                            }))
                            .child(VectorIcon::new(REFRESH_SHAPES, px(13.0)).ink(ui::text_muted())),
                    )
                    .when(header_capabilities.show_upload, |el| {
                        el.child(
                            header_action_button(
                                "file-tree-upload-file",
                                header_capabilities.mutations_enabled,
                            )
                            .tooltip(|window, cx| {
                                Tooltip::new(t("fileTree", "menu.uploadFiles")).build(window, cx)
                            })
                            .when(header_capabilities.mutations_enabled, |el| {
                                el.on_click(cx.listener(|this, _event, window, cx| {
                                    let Some(context) = this.operation_context(cx) else {
                                        return;
                                    };
                                    if this.operation_busy
                                        || !matches!(
                                            &context.backend,
                                            FileBackendIdentity::Remote { .. }
                                        )
                                    {
                                        return;
                                    }
                                    let root = context.root.clone();
                                    choose_upload_paths(
                                        cx.entity(),
                                        context,
                                        root,
                                        false,
                                        window,
                                        cx,
                                    );
                                }))
                            })
                            .child(
                                VectorIcon::new(FILE_SHAPES, px(13.0))
                                    .overlay(UPLOAD_MARK_SHAPES)
                                    .ink(ui::text_muted()),
                            ),
                        )
                        .child(
                            header_action_button(
                                "file-tree-upload-folder",
                                header_capabilities.mutations_enabled,
                            )
                            .tooltip(|window, cx| {
                                Tooltip::new(t("fileTree", "menu.uploadFolder")).build(window, cx)
                            })
                            .when(header_capabilities.mutations_enabled, |el| {
                                el.on_click(cx.listener(|this, _event, window, cx| {
                                    let Some(context) = this.operation_context(cx) else {
                                        return;
                                    };
                                    if this.operation_busy
                                        || !matches!(
                                            &context.backend,
                                            FileBackendIdentity::Remote { .. }
                                        )
                                    {
                                        return;
                                    }
                                    let root = context.root.clone();
                                    choose_upload_paths(
                                        cx.entity(),
                                        context,
                                        root,
                                        true,
                                        window,
                                        cx,
                                    );
                                }))
                            })
                            .child(
                                VectorIcon::new(FOLDER_SHAPES, px(13.0))
                                    .overlay(UPLOAD_MARK_SHAPES)
                                    .ink(ui::text_muted()),
                            ),
                        )
                    })
                    .child(
                        header_action_button("file-tree-paste", header_capabilities.paste_enabled)
                            .tooltip(|window, cx| {
                                Tooltip::new(t("fileTree", "menu.paste")).build(window, cx)
                            })
                            .when(header_capabilities.paste_enabled, |el| {
                                el.on_click(cx.listener(|this, _event, window, cx| {
                                    let Some(context) = this.operation_context(cx) else {
                                        return;
                                    };
                                    if this.operation_busy
                                        || !this
                                            .file_clipboard
                                            .as_ref()
                                            .is_some_and(|clip| clip.can_paste_into(&context))
                                    {
                                        return;
                                    }
                                    let root = context.root.clone();
                                    // paste_file_clipboard 进门会 tree.read；当前 listener
                                    // 仍持有 FileTree 的 update 租约，直接调用会触发 GPUI
                                    // double-lease panic。延后一拍，并由业务入口再次校验
                                    // context/clipboard，避免项目切换时使用旧快照。
                                    let tree = cx.entity();
                                    window.defer(cx, move |window, cx| {
                                        paste_file_clipboard(tree, context, root, window, cx);
                                    });
                                }))
                            })
                            .child(VectorIcon::new(PASTE_SHAPES, px(13.0)).ink(ui::text_muted())),
                    )
                    .child(
                        header_action_button(
                            "file-tree-new-file",
                            header_capabilities.mutations_enabled,
                        )
                        .tooltip(|window, cx| {
                            Tooltip::new(t("fileTree", "menu.newFile")).build(window, cx)
                        })
                        .when(header_capabilities.mutations_enabled, |el| {
                            el.on_click(cx.listener(|this, _event, window, cx| {
                                let Some(context) = this.operation_context(cx) else {
                                    return;
                                };
                                if this.operation_busy
                                    || matches!(&context.backend, FileBackendIdentity::BrokenRemote)
                                {
                                    return;
                                }
                                let root = context.root.clone();
                                let connection = this.remote_conn(cx);
                                new_entry_prompt(
                                    cx.entity(),
                                    context,
                                    connection,
                                    root,
                                    false,
                                    window,
                                    cx,
                                );
                            }))
                        })
                        .child(
                            VectorIcon::new(FILE_SHAPES, px(13.0))
                                .overlay(PLUS_MARK_SHAPES)
                                .ink(ui::text_muted()),
                        ),
                    )
                    .child(
                        header_action_button(
                            "file-tree-new-folder",
                            header_capabilities.mutations_enabled,
                        )
                        .tooltip(|window, cx| {
                            Tooltip::new(t("fileTree", "menu.newFolder")).build(window, cx)
                        })
                        .when(header_capabilities.mutations_enabled, |el| {
                            el.on_click(cx.listener(|this, _event, window, cx| {
                                let Some(context) = this.operation_context(cx) else {
                                    return;
                                };
                                if this.operation_busy
                                    || matches!(&context.backend, FileBackendIdentity::BrokenRemote)
                                {
                                    return;
                                }
                                let root = context.root.clone();
                                let connection = this.remote_conn(cx);
                                new_entry_prompt(
                                    cx.entity(),
                                    context,
                                    connection,
                                    root,
                                    true,
                                    window,
                                    cx,
                                );
                            }))
                        })
                        .child(
                            VectorIcon::new(FOLDER_SHAPES, px(13.0))
                                .overlay(PLUS_MARK_SHAPES)
                                .ink(ui::text_muted()),
                        ),
                    )
                    .when(!is_remote, |el| {
                        el.when_some(default_editor.clone(), |el, current| {
                            el.child(self.render_editor_picker(current, editors.clone(), cx))
                        })
                    }),
            );
        }

        let Some(project_id) = self.current_project.clone() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(ui::font_px(11.4))
                        .text_color(ui::text_muted())
                        .child(t("fileTree", "empty.selectProject")),
                );
        };
        let Some(root) = self.project_root(cx) else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header);
        };

        // 展开态与 `entries` 缓存的对账:展开着却没列过的目录在这儿补列回来
        // (换项目/面板重建/冷启动都会把缓存清掉,展开状态却是落盘的)。
        // `load_dir` 自带「同目录不重复排队」的闸门 —— 排队的那几帧重复走到这儿
        // 只多查一次 HashSet
        {
            let mut missing = Vec::new();
            {
                let store = self.store.read(cx);
                let is_expanded = |path: &Path| {
                    store.is_dir_expanded(&project_id, path.to_string_lossy().as_ref())
                };
                missing_expanded_dirs(&self.entries, &root, &is_expanded, &mut missing);
            }
            missing.retain(|dir| !self.path_is_suppressed(dir));
            for dir in missing {
                self.load_dir(root.clone(), dir, cx);
            }
        }

        let mut rows = Vec::new();
        self.rows(&project_id, &root, &root, 0, cx, &mut rows);
        // 行焦点句柄按**当前可见行**补齐并回收(折叠掉的行不必留着句柄)。
        // 句柄要跨帧稳定 —— 每帧新建的话 Tab 过去的焦点每帧都会丢
        {
            let visible: HashSet<&PathBuf> = rows.iter().map(|r| &r.path).collect();
            self.row_focus.retain(|path, _| visible.contains(path));
            let missing: Vec<PathBuf> = rows
                .iter()
                .filter(|r| !self.row_focus.contains_key(&r.path))
                .map(|r| r.path.clone())
                .collect();
            for path in missing {
                self.row_focus(&path, cx);
            }
        }

        // 断链的远程项目:静态提示,**没有重试按钮**(连接都没了,重试也是白试)
        if self.remote_broken {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .py(px(32.0))
                        .px(px(12.0))
                        .text_size(ui::font_px(11.4))
                        .text_color(ui::color_error())
                        .child(t("fileTree", "remote.broken")),
                );
        }

        // 三态占位:**都以「一行都没有」为前置** —— 有缓存内容时不整块盖掉
        if rows.is_empty() && (self.root_loading || self.root_error.is_some()) {
            let body = if self.root_loading {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(32.0))
                    .text_size(ui::font_px(11.4))
                    .text_color(ui::text_muted())
                    .child(t("fileTree", "empty.loading"))
            } else {
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .py(px(32.0))
                    .px(px(12.0))
                    .text_size(ui::font_px(11.4))
                    .child(
                        div()
                            .truncate()
                            .text_color(ui::text_muted())
                            .child(t("fileTree", "empty.loadFailed")),
                    )
                    .child(
                        div()
                            .id("file-tree-retry")
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(3.0))
                            .cursor_pointer()
                            .text_color(ui::accent())
                            .hover(|el| el.bg(ui::border_subtle()))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.refresh_root(cx);
                            }))
                            .child(t("fileTree", "empty.retry")),
                    )
            };
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(header)
                .child(body);
        }

        let mut list = div()
            .id("file-tree-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll();
        for row in rows {
            list = list.child(self.render_row(row, cx));
        }
        let background_target = root.clone();
        let background_drop_id = "background".to_string();
        let background_context = self.operation_context(cx);
        let background_highlight = self
            .external_drop_target
            .as_ref()
            .is_some_and(|active| active.hit_id == background_drop_id);
        list = list.child(
            div()
                .id("file-tree-background")
                .flex_1()
                .min_h(px(24.0))
                .when(background_highlight, |el| {
                    el.bg(ui::with_alpha(ui::accent(), 0.12))
                })
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        let Some(context) = this.operation_context(cx) else {
                            return;
                        };
                        let can_paste = this
                            .file_clipboard
                            .as_ref()
                            .is_some_and(|clipboard| clipboard.can_paste_into(&context));
                        let store = this.store.clone();
                        let entries = background_menu(
                            &cx.entity(),
                            &store,
                            context,
                            this.remote_conn(cx),
                            can_paste,
                        );
                        menu::show(event.position, entries, window, cx);
                    }),
                )
                .when(is_remote, |el| {
                    let move_target = background_target.clone();
                    let drop_target = background_target.clone();
                    let move_id = background_drop_id.clone();
                    let drop_context = background_context.clone();
                    el.on_drag_move(cx.listener(
                        move |this, event: &DragMoveEvent<ExternalPaths>, _window, cx| {
                            this.note_external_drop_target(&move_id, &move_target, event, cx);
                        },
                    ))
                    .on_drop(cx.listener(
                        move |this, paths: &ExternalPaths, window, cx| {
                            this.external_drop_target = None;
                            cx.notify();
                            let Some(context) = drop_context.clone() else {
                                return;
                            };
                            // listener 里 FileTree 正被 update,start_upload 一进门就
                            // tree.read 会 double-lease panic;且此刻栈在 OLE Drop 的
                            // COM 回调上,panic 不可展开、整个进程直接 abort。defer 到
                            // 租约释放后再跑。
                            let tree = cx.entity();
                            let target = drop_target.clone();
                            let paths = paths.paths().to_vec();
                            window.defer(cx, move |window, cx| {
                                start_upload(tree, context, target, paths, window, cx);
                            });
                        },
                    ))
                }),
        );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(header)
            // 有旧内容时的刷新失败:一条细提示挂在列表**上方**,内容照旧留着
            .when_some(self.root_error.clone(), |el, _err| {
                el.child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .truncate()
                        .text_size(ui::font_px(9.75))
                        .text_color(ui::text_muted())
                        .child(t("fileTree", "empty.refreshFailed")),
                )
            })
            .when_some(self.operation_label.clone(), |el, label| {
                el.child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .truncate()
                        .text_size(ui::font_px(10.5))
                        .text_color(ui::accent())
                        .child(label),
                )
            })
            .child(list)
    }
}

impl FileTree {
    /// 头部的编辑器分裂按钮:左半边用默认编辑器打开项目根,右半边(多于一个
    /// 编辑器时才有)弹出选择菜单 —— 选中项**先把 `defaultEditor` 改掉并落盘,
    /// 再打开**(原版 `handleSwitchAndOpen`,`FileTree.tsx:462-467`)。
    fn render_editor_picker(
        &self,
        current: String,
        editors: Vec<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_default = cx.listener(|this, _event, _window, cx| {
            let Some(root) = this.project_root(cx) else {
                return;
            };
            let editor = fs_ops::configured_editor(this.store.read(cx).config());
            fs_ops::open_path_with(editor, root, cx);
        });

        let mut picker = div()
            .flex()
            .items_center()
            .ml(px(2.0))
            .pl(px(4.0))
            .border_l_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .id("file-tree-editor")
                    .h(px(26.0))
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(9.75))
                    .text_color(ui::text_muted())
                    .hover(|el| el.text_color(ui::text_primary()).bg(ui::border_subtle()))
                    .tooltip({
                        let editor = current.clone();
                        move |window, cx| {
                            Tooltip::new(tr!("fileTree", "header.openWithEditor", editor = editor))
                                .build(window, cx)
                        }
                    })
                    .on_click(open_default)
                    .child(current.clone()),
            );

        if editors.len() > 1 {
            let this = cx.entity();
            picker = picker.child(
                div()
                    .id("file-tree-editor-more")
                    .h(px(26.0))
                    .pl(px(4.0))
                    .pr(px(6.0))
                    .flex()
                    .items_center()
                    .rounded(px(3.0))
                    .border_l_1()
                    .border_color(ui::border_subtle())
                    .cursor_pointer()
                    .text_color(ui::text_muted())
                    .hover(|el| el.text_color(ui::text_primary()).bg(ui::border_subtle()))
                    .tooltip(|window, cx| {
                        Tooltip::new(t("fileTree", "menu.chooseOtherEditor")).build(window, cx)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, cx| {
                            let entries: Vec<MenuEntry> = editors
                                .iter()
                                .map(|name| {
                                    // 当前默认项尾部加 ` (*)`(原版就是这个字面量)
                                    let label = if *name == current {
                                        format!("{name} (*)")
                                    } else {
                                        name.clone()
                                    };
                                    let this = this.clone();
                                    let pick = name.clone();
                                    menu::item(label, move |_window, cx| {
                                        this.update(cx, |tree: &mut FileTree, cx| {
                                            let name = pick.clone();
                                            tree.store.update(cx, |store, cx| {
                                                store.patch_config(
                                                    |config| config.default_editor = Some(name),
                                                    cx,
                                                );
                                            });
                                            let Some(root) = tree.project_root(cx) else {
                                                return;
                                            };
                                            let editor = fs_ops::configured_editor(
                                                tree.store.read(cx).config(),
                                            );
                                            fs_ops::open_path_with(editor, root, cx);
                                        });
                                    })
                                })
                                .collect();
                            menu::show(event.position, entries, window, cx);
                        },
                    )
                    .child(VectorIcon::new(CARET_SHAPES, px(8.0)).ink(ui::text_muted())),
            );
        }
        picker.into_any_element()
    }

    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let path = row.path.clone();
        let is_dir = row.is_dir;
        let row_for_menu = row.clone();
        let drag_path = row.path.clone();
        let drag_name = row.name.clone();
        let drag_is_dir = row.is_dir;
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
        // 语言色。**git 状态着的是行尾那枚状态字母,不是文件名本身的颜色**
        // (`FileTree.tsx:565` 的注释专门点了这条)。
        let git_badge = row.git.clone();
        // 一级子工程目录优先显示技术栈徽标(原版那段 IIFE 的第一条分支)
        let icon: AnyElement = match row.kind {
            Some(kind) => mt_ui::icons::TechIcon::new(kind)
                .size(px(14.0))
                .into_any_element(),
            None => {
                let icon = FileIcon::new(&row.name, row.is_dir, row.expanded).size(px(14.0));
                if row.ignored {
                    icon.color(ui::text_muted()).into_any_element()
                } else {
                    icon.into_any_element()
                }
            }
        };
        let focus = self.row_focus.get(&row.path).cloned();
        let key_path = row.path.clone();
        let key_expanded = row.expanded;
        let remote = self.is_remote(cx);
        let row_context = self.operation_context(cx);
        let key_context = row_context.clone();
        let click_context = row_context.clone();
        let drop_context = row_context.clone();
        let upload_target = if row.is_dir {
            row.path.clone()
        } else {
            crate::remote_ssh::parent_posix(&row.path.to_string_lossy())
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_root(cx).unwrap_or_else(|| row.path.clone()))
        };
        let row_drop_id = format!("row:{}", row.path.display());
        let drop_highlight = self
            .external_drop_target
            .as_ref()
            .is_some_and(|active| active.hit_id == row_drop_id);

        div()
            .id(SharedString::from(format!("fs-{}", row.path.display())))
            // 行级焦点 + tab 停靠点(原版每行 `tabIndex={0}` + `role=treeitem`)
            .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if this.operation_context(cx).as_ref() != key_context.as_ref() {
                        return;
                    }
                    this.on_row_key(event, &key_path, is_dir, key_expanded, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let path = row.path.clone();
                    move |this, _event: &MouseDownEvent, window, _cx| {
                        // 浏览器点 `tabIndex=0` 的行就会聚焦,←→ 折叠展开靠这一条才够得着
                        if let Some(focus) = this.row_focus.get(&path) {
                            window.focus(focus);
                        }
                    }
                }),
            )
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
            .when(drop_highlight, |el| {
                el.bg(ui::with_alpha(ui::accent(), 0.18))
            })
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    if this.operation_context(cx).as_ref() != click_context.as_ref() {
                        return;
                    }
                    if is_dir {
                        this.toggle_dir(path.clone(), cx);
                    } else if event.click_count() <= 1 {
                        // 单击开预览器;双击的第二个事件(click_count == 2)不再做别的,
                        // 见 `open_file` 的注释
                        this.open_file(path.clone(), window, cx);
                    }
                }),
            )
            // 拖进终端 = 把路径当文本写进 PTY(不是上传文件)。目录同样可拖,
            // 与原版一致(`FileTree.tsx:326-328` 的 `initFileDrag(entry.path)`
            // 不区分文件/目录)。落点在 `terminal_area.rs` 的 pane 主体。
            //
            // 原版为此自研了一整套 pointer 跟踪 + `body.file-dragging` 的
            // `pointer-events:none` 穿透规则(要让鼠标穿过 xterm 的子 DOM 打到
            // drop-zone 上);gpui 侧终端是自绘 Element、drop 目标就是它的容器,
            // 那条穿透规则一行都不必移植。
            .on_drag(
                crate::dnd::DragFilePath(drag_path),
                move |_item, _offset, _window, cx| {
                    crate::dnd::preview(
                        drag_name.clone(),
                        crate::dnd::PreviewIcon::File {
                            name: drag_name.clone(),
                            is_dir: drag_is_dir,
                        },
                        cx,
                    )
                },
            )
            .when(remote, |el| {
                let move_target = upload_target.clone();
                let drop_target = upload_target.clone();
                let move_id = row_drop_id.clone();
                let drop_context = drop_context.clone();
                el.on_drag_move(cx.listener(
                    move |this, event: &DragMoveEvent<ExternalPaths>, _window, cx| {
                        this.note_external_drop_target(&move_id, &move_target, event, cx);
                    },
                ))
                .on_drop(cx.listener(
                    move |this, paths: &ExternalPaths, window, cx| {
                        this.external_drop_target = None;
                        cx.notify();
                        let Some(context) = drop_context.clone() else {
                            return;
                        };
                        // 同背景落点:实体租约未释放前不能进 start_upload(double-lease
                        // 会在 COM 拖放栈上 abort),defer 一拍。
                        let tree = cx.entity();
                        let target = drop_target.clone();
                        let paths = paths.paths().to_vec();
                        window.defer(cx, move |window, cx| {
                            start_upload(tree, context, target, paths, window, cx);
                        });
                    },
                ))
            })
            // 行的右键菜单。**必须 stop_propagation** —— 否则会连带触发列表容器
            // 那个「空白处右键 = 新建」的菜单(原版靠 `e.stopPropagation()` 同理)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(context) = row_context.clone() else {
                        return;
                    };
                    if this.operation_context(cx).as_ref() != Some(&context) {
                        return;
                    }
                    let store = this.store.clone();
                    let connection = this.remote_conn(cx);
                    let can_paste = this.file_clipboard.as_ref().is_some_and(|clipboard| {
                        clipboard.can_paste_into(&context)
                            && !clipboard.would_copy_into_itself(&row_for_menu.path)
                    });
                    let entries = file_menu(
                        &cx.entity(),
                        &store,
                        &row_for_menu,
                        context,
                        connection,
                        can_paste,
                    );
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
            .child(div().flex_1().truncate().child(row.name))
            // git 状态字母。目录那枚是**汇总**来的,画淡一档以示区别(原版 opacity-70)
            .when_some(git_badge, |el, (label, rolled_up)| {
                el.child(
                    div()
                        .flex_none()
                        .ml(px(6.0))
                        .text_size(ui::font_px(9.75))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(if rolled_up {
                            ui::with_alpha(git_color(&label), 0.7)
                        } else {
                            git_color(&label)
                        })
                        .child(label),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::FileMenuAction::*;
    use super::*;

    #[test]
    fn 远程下载上下文要求项目根目录和连接身份完全一致() {
        let context = FileOperationContext {
            project_id: "project-a".into(),
            root: PathBuf::from("/workspace"),
            backend: FileBackendIdentity::Remote {
                connection_id: "ssh-a".into(),
                connection_fingerprint: 7,
            },
            generation: 3,
        };
        assert!(remote_download_context_matches(
            &context,
            "project-a",
            "/workspace",
            "ssh-a",
            7,
        ));
        assert!(!remote_download_context_matches(
            &context,
            "project-b",
            "/workspace",
            "ssh-a",
            7,
        ));
        assert!(!remote_download_context_matches(
            &context,
            "project-a",
            "/other",
            "ssh-a",
            7,
        ));
        assert!(!remote_download_context_matches(
            &context,
            "project-a",
            "/workspace",
            "ssh-b",
            7,
        ));
        assert!(!remote_download_context_matches(
            &context,
            "project-a",
            "/workspace",
            "ssh-a",
            8,
        ));

        for backend in [
            FileBackendIdentity::Local,
            FileBackendIdentity::BrokenRemote,
        ] {
            let context = FileOperationContext {
                backend,
                ..context.clone()
            };
            assert!(!remote_download_context_matches(
                &context,
                "project-a",
                "/workspace",
                "ssh-a",
                7,
            ));
        }
    }

    /// 文件的菜单:「使用默认工具打开」在最前(原版 unshift),没有「新建」两项。
    ///
    /// ⚠️ Y 批把「查看变更」接了上去(V 批的 `open_file_diff` 已就绪),
    /// 于是这条断言的期望向量**多了尾部两项**(分隔线 + ViewDiff);
    /// 没有 git 状态的文件仍然与从前一模一样,见下面那条。
    #[test]
    fn 文件菜单项序与原版一致() {
        assert_eq!(
            file_menu_actions(false, true, false),
            vec![
                Some(OpenWithDefault),
                Some(CopyEntry),
                None,
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                Some(RevealInFolder),
                Some(OpenInTerminal),
                None,
                Some(Rename),
                Some(Delete),
                None,
                Some(ViewDiff),
            ]
        );
        // 干净文件:一项不多(原版 `entryGitStatus && !entry.isDir`)
        assert_eq!(
            file_menu_actions(false, false, false),
            vec![
                Some(OpenWithDefault),
                Some(CopyEntry),
                None,
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                Some(RevealInFolder),
                Some(OpenInTerminal),
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
            file_menu_actions(true, false, false),
            vec![
                Some(CopyEntry),
                Some(Paste),
                None,
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                Some(RevealInFolder),
                Some(OpenInTerminal),
                None,
                Some(Rename),
                Some(Delete),
                None,
                Some(NewFile),
                Some(NewFolder),
            ]
        );
    }

    /// 「查看变更」只给**有 git 状态的文件**:目录哪怕汇总出了字母也不给
    /// (原版判定是 `entryGitStatus && !entry.isDir`,单文件 diff 对目录没意义);
    /// 而「默认工具打开」只对文件出现。
    #[test]
    fn 目录与文件的差别只在两处() {
        let file: Vec<_> = file_menu_actions(false, false, false)
            .into_iter()
            .flatten()
            .collect();
        let dir: Vec<_> = file_menu_actions(true, false, false)
            .into_iter()
            .flatten()
            .collect();
        assert!(file.contains(&OpenWithDefault));
        assert!(!dir.contains(&OpenWithDefault));
        assert!(dir.contains(&NewFile) && dir.contains(&NewFolder));
        assert!(!file.contains(&NewFile) && !file.contains(&NewFolder));
        // 有状态的目录同样不给 ViewDiff
        let dirty_dir: Vec<_> = file_menu_actions(true, true, false)
            .into_iter()
            .flatten()
            .collect();
        assert!(!dirty_dir.contains(&ViewDiff));
        assert_eq!(dirty_dir, dir);
    }

    #[test]
    fn 远程菜单不暴露本机动作并提供传输入口() {
        assert_eq!(
            file_menu_actions(false, true, true),
            vec![
                Some(CopyEntry),
                Some(Download),
                None,
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                Some(OpenInTerminal),
                None,
                Some(Rename),
                Some(Delete),
            ]
        );
        assert_eq!(
            file_menu_actions(true, false, true),
            vec![
                Some(CopyEntry),
                Some(Paste),
                Some(Download),
                Some(UploadFiles),
                Some(UploadFolder),
                None,
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                Some(OpenInTerminal),
                None,
                Some(Rename),
                Some(Delete),
                None,
                Some(NewFile),
                Some(NewFolder),
            ]
        );
    }

    // ─── git 状态着色 ─────────────────────────────────────────

    /// 六个字母的配色逐条对照 `FileTree.tsx:362-369`,认不出的退 muted。
    #[test]
    fn git状态配色照抄原版() {
        assert_eq!(git_color("M"), ui::color_warning());
        assert_eq!(git_color("A"), ui::color_success());
        // 未跟踪与新增同色(原版 `'?': text-success`)
        assert_eq!(git_color("?"), ui::color_success());
        assert_eq!(git_color("D"), ui::color_error());
        assert_eq!(git_color("C"), ui::color_error());
        assert_eq!(git_color("R"), ui::color_info());
        // 后端将来加了新字母也不会画成错的颜色
        assert_eq!(git_color("X"), ui::text_muted());
        assert_eq!(git_color(""), ui::text_muted());
    }

    /// 目录汇总取子树里优先级最高的那个字母,且**只认前缀是自己的**条目。
    #[test]
    fn 目录汇总取最高优先级() {
        let map: HashMap<String, String> = [
            ("src/a.rs", "M"),
            ("src/b.rs", "C"),
            ("src/deep/c.rs", "A"),
            // 同名前缀的兄弟目录不许被算进来
            ("srcx/d.rs", "D"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        assert_eq!(rollup_dir_label(&map, "src"), Some("C"));
        assert_eq!(rollup_dir_label(&map, "src/deep"), Some("A"));
        assert_eq!(rollup_dir_label(&map, "srcx"), Some("D"));
        // 没有子项的目录不出徽章
        assert_eq!(rollup_dir_label(&map, "docs"), None);
        // 文件自身那条不算「子树」(前缀要带 `/`)
        assert_eq!(rollup_dir_label(&map, "src/a.rs"), None);
    }

    // ─── 单链目录压缩 ─────────────────────────────────────────

    fn entry(name: &str, path: &str, is_dir: bool, ignored: bool) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: PathBuf::from(path),
            is_dir,
            ignored,
        }
    }

    /// 假目录表:`路径 → 子项`。
    fn faker(table: Vec<(&'static str, Vec<FileEntry>)>) -> impl FnMut(&Path) -> Vec<FileEntry> {
        let table: HashMap<PathBuf, Vec<FileEntry>> = table
            .into_iter()
            .map(|(k, v)| (PathBuf::from(k), v))
            .collect();
        move |dir: &Path| table.get(dir).cloned().unwrap_or_default()
    }

    /// 一路单子目录 → 折成一行,名字用 `/` 拼,路径指向**链尾**。
    #[test]
    fn 单链目录折成一行() {
        let entries = vec![entry("src", "/p/src", true, false)];
        let list = faker(vec![
            ("/p/src", vec![entry("main", "/p/src/main", true, false)]),
            (
                "/p/src/main",
                vec![entry("java", "/p/src/main/java", true, false)],
            ),
            // 链尾有两个子项 → 停
            (
                "/p/src/main/java",
                vec![
                    entry("A.java", "/p/src/main/java/A.java", false, false),
                    entry("B.java", "/p/src/main/java/B.java", false, false),
                ],
            ),
        ]);
        let out = compact_dir_chains(entries, list);
        assert_eq!(out.len(), 1);
        let (entry, chain) = &out[0];
        assert_eq!(entry.name, "src/main/java");
        assert_eq!(entry.path, PathBuf::from("/p/src/main/java"));
        assert_eq!(
            chain,
            &vec![
                PathBuf::from("/p/src"),
                PathBuf::from("/p/src/main"),
                PathBuf::from("/p/src/main/java"),
            ]
        );
    }

    /// 不压缩的几种:文件 / 被忽略的目录 / 唯一子项是文件 / 唯一子项被忽略。
    /// 这几种**都返回长度 1 的 chain**(调用方据此不登记链、不额外挂监听)。
    #[test]
    fn 不满足前提时原样返回() {
        let entries = vec![
            entry("readme.md", "/p/readme.md", false, false),
            entry("target", "/p/target", true, true),
            entry("only-file", "/p/only-file", true, false),
            entry("only-ignored", "/p/only-ignored", true, false),
        ];
        let list = faker(vec![
            (
                "/p/only-file",
                vec![entry("a.txt", "/p/only-file/a.txt", false, false)],
            ),
            (
                "/p/only-ignored",
                vec![entry(
                    "node_modules",
                    "/p/only-ignored/node_modules",
                    true,
                    true,
                )],
            ),
            // 被忽略的目录压根不该被列(命中就说明闸门漏了)
            ("/p/target", vec![entry("x", "/p/target/x", true, false)]),
        ]);
        let out = compact_dir_chains(entries, list);
        for (entry, chain) in &out {
            assert_eq!(chain.len(), 1, "{} 不该被压缩", entry.name);
            assert!(!entry.name.contains('/'), "{} 不该改名", entry.name);
        }
    }

    /// 链深上限 8:再深也不继续列(每层一次串行 IPC)。
    #[test]
    fn 链深封顶八层() {
        // /p/d0 → d1 → … 无限深
        let mut table: Vec<(&'static str, Vec<FileEntry>)> = Vec::new();
        const PATHS: [&str; 12] = [
            "/p/d0", "/p/d1", "/p/d2", "/p/d3", "/p/d4", "/p/d5", "/p/d6", "/p/d7", "/p/d8",
            "/p/d9", "/p/d10", "/p/d11",
        ];
        for (i, path) in PATHS.iter().enumerate().take(PATHS.len() - 1) {
            let next = PATHS[i + 1];
            let name = next.rsplit('/').next().unwrap();
            table.push((path, vec![entry(name, next, true, false)]));
        }
        let out = compact_dir_chains(vec![entry("d0", "/p/d0", true, false)], faker(table));
        let (entry, chain) = &out[0];
        assert_eq!(chain.len(), MAX_CHAIN);
        assert_eq!(entry.name, "d0/d1/d2/d3/d4/d5/d6/d7");
        assert_eq!(entry.path, PathBuf::from("/p/d7"));
    }

    // ─── 展开态与缓存的对账 ───────────────────────────────────

    /// `entries` 缓存表:`目录 → 子项`。
    fn listed(table: Vec<(&'static str, Vec<FileEntry>)>) -> HashMap<PathBuf, Vec<FileEntry>> {
        table
            .into_iter()
            .map(|(k, v)| (PathBuf::from(k), v))
            .collect()
    }

    fn expanded_set(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    /// 换项目回来的那一刻:只有根列过,展开着的一级目录全要补列。
    #[test]
    fn 展开却没列过的目录要补列() {
        let entries = listed(vec![(
            "/p",
            vec![
                entry("src", "/p/src", true, false),
                entry("docs", "/p/docs", true, false),
                entry("readme.md", "/p/readme.md", false, false),
            ],
        )]);
        let mut out = Vec::new();
        missing_expanded_dirs(
            &entries,
            Path::new("/p"),
            &expanded_set(&["/p/src"]),
            &mut out,
        );
        // 折叠的 docs 与文件 readme.md 都不掺和
        assert_eq!(out, vec![PathBuf::from("/p/src")]);
    }

    /// 已列过的目录不重复排队,但要**顺着它往下**继续对账。
    #[test]
    fn 已列过的目录只往下走() {
        let entries = listed(vec![
            ("/p", vec![entry("src", "/p/src", true, false)]),
            ("/p/src", vec![entry("core", "/p/src/core", true, false)]),
        ]);
        let mut out = Vec::new();
        missing_expanded_dirs(
            &entries,
            Path::new("/p"),
            &expanded_set(&["/p/src", "/p/src/core"]),
            &mut out,
        );
        assert_eq!(out, vec![PathBuf::from("/p/src/core")]);
    }

    /// 一轮只补**下一层**:祖先自己都还没列回来时,深层那条陈旧展开记录翻不到 ——
    /// 远程一次列目录是一趟 SFTP 往返,不能按 `expandedDirs` 整份去列。
    #[test]
    fn 祖先没列出来时不越级补列() {
        let entries = listed(vec![("/p", vec![entry("src", "/p/src", true, false)])]);
        let mut out = Vec::new();
        missing_expanded_dirs(
            &entries,
            Path::new("/p"),
            // /p/src/core 也是展开的,但 /p/src 这一层还没内容,够不着
            &expanded_set(&["/p/src", "/p/src/core"]),
            &mut out,
        );
        assert_eq!(out, vec![PathBuf::from("/p/src")]);
    }

    /// 列失败时那条空记录(见 `load_dir_with` 的 Err 分支)让补列就此打住 ——
    /// 否则 render → 补列 → 失败 → notify → render 会绕成死循环。
    #[test]
    fn 列过的空目录不再重排() {
        let entries = listed(vec![
            ("/p", vec![entry("src", "/p/src", true, false)]),
            ("/p/src", Vec::new()),
        ]);
        let mut out = Vec::new();
        missing_expanded_dirs(
            &entries,
            Path::new("/p"),
            &expanded_set(&["/p/src"]),
            &mut out,
        );
        assert!(out.is_empty());
    }

    /// 根目录自己都还没列出来(冷启动第一帧)时一条都不补:根那趟由
    /// `sync_project` / `refresh_root` 显式排,补列不插手。
    #[test]
    fn 根没列出来时什么都不补() {
        let mut out = Vec::new();
        missing_expanded_dirs(
            &HashMap::new(),
            Path::new("/p"),
            &expanded_set(&["/p/src"]),
            &mut out,
        );
        assert!(out.is_empty());
    }

    /// 优先级表逐条(`PRIORITY = {C:6, D:5, M:4, A:3, R:2, '?':1}`)。
    #[test]
    fn 汇总优先级与原版一致() {
        let order = ["C", "D", "M", "A", "R", "?"];
        for pair in order.windows(2) {
            assert!(
                git_priority(pair[0]) > git_priority(pair[1]),
                "{} 应当排在 {} 前面",
                pair[0],
                pair[1]
            );
        }
        // 认不出的字母不参与汇总(优先级 0)
        assert_eq!(git_priority("X"), 0);
    }

    #[test]
    fn 文件树头部动作按后端和忙碌状态收口() {
        let local = FileOperationContext {
            project_id: "p".into(),
            root: PathBuf::from("/work"),
            backend: FileBackendIdentity::Local,
            generation: 1,
        };
        let clip = FileClipboardEntry {
            project_id: "p".into(),
            root: PathBuf::from("/work"),
            backend: FileBackendIdentity::Local,
            generation: 1,
            source: PathBuf::from("/work/a.txt"),
            is_dir: false,
        };
        assert_eq!(
            header_action_capabilities(Some(&local), false, Some(&clip)),
            HeaderActionCapabilities {
                show_upload: false,
                mutations_enabled: true,
                paste_enabled: true,
            }
        );
        assert_eq!(
            header_action_capabilities(Some(&local), true, Some(&clip)),
            HeaderActionCapabilities {
                show_upload: false,
                mutations_enabled: false,
                paste_enabled: false,
            }
        );

        let mut remote = local.clone();
        remote.backend = FileBackendIdentity::Remote {
            connection_id: "ssh".into(),
            connection_fingerprint: 7,
        };
        let remote_caps = header_action_capabilities(Some(&remote), false, None);
        assert!(remote_caps.show_upload && remote_caps.mutations_enabled);
        assert!(!remote_caps.paste_enabled);

        let mut broken = remote;
        broken.backend = FileBackendIdentity::BrokenRemote;
        assert_eq!(
            header_action_capabilities(Some(&broken), false, None),
            HeaderActionCapabilities {
                show_upload: false,
                mutations_enabled: false,
                paste_enabled: false,
            }
        );
    }
}

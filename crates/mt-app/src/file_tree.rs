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
    AnyElement, App, ClipboardItem, Context, Entity, Hsla, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use mt_ui::tooltip::Tooltip;
use mt_project::fs::FileEntry;
use mt_project::watch::FsWatcher;
use mt_ui::icons::FileIcon;
use mt_ui::icons::vector::{Geom, Ink, Shape, VectorIcon};

use crate::fs_ops;
use crate::git_watch;
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
            watcher,
            watched: HashSet::new(),
            current_project: None,
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
        let Some(root) = self.project_root(cx) else {
            self.git_status.clear();
            return;
        };
        cx.spawn(async move |this, cx| {
            let probe = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_git_status(&probe) })
                .await;
            let _ = this.update(cx, |tree: &mut FileTree, cx| {
                // 回来时项目可能已经换掉了 —— 只认还对得上号的那一次
                if tree.project_root(cx).as_deref() != Some(root.as_path()) {
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
        let (project_id, root, remote, broken) = {
            let store = self.store.read(cx);
            match store.active_project() {
                Some(p) => {
                    let is_remote = store.is_remote_project(&p.id);
                    let conn = store.remote_connection_of(&p.id);
                    (
                        Some(p.id.clone()),
                        Some(PathBuf::from(&p.path)),
                        conn.is_some(),
                        // 断链 = 是远程项目但连接查不到
                        is_remote && conn.is_none(),
                    )
                }
                None => (None, None, false, false),
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
        self.chain_owner.clear();
        self.git_status.clear();
        self.git_refresh_at = None;
        self.root_error = None;
        self.current_project = project_id;
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

    /// 列一个目录(后台线程)+ 挂监听。
    ///
    /// `refresh_ignore` **只对远程有效**:强制后端重读远程根 `.gitignore`
    /// (头部刷新按钮那一路,原版 `loadRootEntries(true)`)。
    fn load_dir(&mut self, root: PathBuf, dir: PathBuf, cx: &mut Context<Self>) {
        self.load_dir_with(root, dir, false, cx);
    }

    fn load_dir_with(
        &mut self,
        root: PathBuf,
        dir: PathBuf,
        refresh_ignore: bool,
        cx: &mut Context<Self>,
    ) {
        if self.loading.contains(&dir) {
            return;
        }
        self.loading.insert(dir.clone());

        // 远程项目**不注册 watcher**:远端文件系统本机监听不到
        let remote = self.remote_conn(cx);
        if remote.is_none()
            && self.watched.insert(dir.clone())
            && let Err(err) = self
                .watcher
                .watch(&dir, &root.to_string_lossy().to_string())
        {
            eprintln!("[files] 监听 {} 失败: {err:#}", dir.display());
        }

        let task_dir = dir.clone();
        let task_root = root.clone();
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
                        return anyhow::Ok(
                            entries.into_iter().map(|e| (e, Vec::new())).collect(),
                        );
                    }
                    let chains = compact_dir_chains(entries, |d| {
                        mt_project::fs::list_directory(&task_root, d).unwrap_or_default()
                    });
                    anyhow::Ok(chains)
                })
                .await;
            let _ = this.update(cx, |tree: &mut FileTree, cx| {
                tree.loading.remove(&dir);
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
        self.load_dir_with(root.clone(), root, remote, cx);
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
        self.load_dir(root, target, cx);
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
        let Some(root) = self.project_root(cx) else {
            return;
        };
        crate::file_viewer::open(root, path, None, window, cx);
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
    CopyRelativePath,
    CopyAbsolutePath,
    RevealInFolder,
    Rename,
    Delete,
    NewFile,
    NewFolder,
    ViewDiff,
}

fn file_menu_actions(is_dir: bool, has_git_status: bool) -> Vec<Option<FileMenuAction>> {
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
    // 目录没有单文件 diff 可看 —— 原版这条判定是 `entryGitStatus && !entry.isDir`
    if !is_dir && has_git_status {
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
fn file_menu(
    tree: &Entity<FileTree>,
    store: &Entity<AppStore>,
    row: &Row,
    root: PathBuf,
) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for action in file_menu_actions(row.is_dir, row.git.is_some()) {
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

/// 快捷键提示里的修饰键名(与 `search_modal` 那份同规则)。
fn mod_label() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }
}

/// 头部那三个 26×26 的图标钮共用的外观(`FileTree.tsx:734`)。
fn header_button(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(26.0))
        .h(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        .cursor_pointer()
        .text_color(ui::text_muted())
        .hover(|el| el.text_color(ui::text_primary()).bg(ui::border_subtle()))
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

/// 编辑器选择器的下拉箭头(原版 8×8 的 `M1.5 3L4 5.5L6.5 3`)。
const CARET_SHAPES: &[Shape] = &[Shape::line(
    Ink::Current,
    0.15,
    Geom::Polyline(&[(0.1875, 0.375), (0.5, 0.6875), (0.8125, 0.375)]),
)];

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = self
            .store
            .read(cx)
            .active_project()
            .map(|p| p.name.clone());
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

        let is_remote = self.is_remote(cx);
        let mut header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .flex_none()
            .px(px(10.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .flex_1()
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
                    .child(
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
                            .child(
                                VectorIcon::new(REFRESH_SHAPES, px(13.0)).ink(ui::text_muted()),
                            ),
                    )
                    .when_some(default_editor.clone(), |el, current| {
                        el.child(self.render_editor_picker(current, editors.clone(), cx))
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

        div()
            .id(SharedString::from(format!("fs-{}", row.path.display())))
            // 行级焦点 + tab 停靠点(原版每行 `tabIndex={0}` + `role=treeitem`)
            .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_row_key(event, &key_path, is_dir, key_expanded, window, cx);
            }))
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
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                if is_dir {
                    this.toggle_dir(path.clone(), cx);
                } else if event.click_count() <= 1 {
                    // 单击开预览器;双击的第二个事件(click_count == 2)不再做别的,
                    // 见 `open_file` 的注释
                    this.open_file(path.clone(), window, cx);
                }
            }))
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
            // 行的右键菜单。**必须 stop_propagation** —— 否则会连带触发列表容器
            // 那个「空白处右键 = 新建」的菜单(原版靠 `e.stopPropagation()` 同理)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(root) = this.project_root(cx) else {
                        return;
                    };
                    let store = this.store.clone();
                    let entries = file_menu(&cx.entity(), &store, &row_for_menu, root);
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

    /// 文件的菜单:「使用默认工具打开」在最前(原版 unshift),没有「新建」两项。
    ///
    /// ⚠️ Y 批把「查看变更」接了上去(V 批的 `open_file_diff` 已就绪),
    /// 于是这条断言的期望向量**多了尾部两项**(分隔线 + ViewDiff);
    /// 没有 git 状态的文件仍然与从前一模一样,见下面那条。
    #[test]
    fn 文件菜单项序与原版一致() {
        assert_eq!(
            file_menu_actions(false, true),
            vec![
                Some(OpenWithDefault),
                Some(CopyRelativePath),
                Some(CopyAbsolutePath),
                None,
                Some(RevealInFolder),
                None,
                Some(Rename),
                Some(Delete),
                None,
                Some(ViewDiff),
            ]
        );
        // 干净文件:一项不多(原版 `entryGitStatus && !entry.isDir`)
        assert_eq!(
            file_menu_actions(false, false),
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
            file_menu_actions(true, false),
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

    /// 「查看变更」只给**有 git 状态的文件**:目录哪怕汇总出了字母也不给
    /// (原版判定是 `entryGitStatus && !entry.isDir`,单文件 diff 对目录没意义);
    /// 而「默认工具打开」只对文件出现。
    #[test]
    fn 目录与文件的差别只在两处() {
        let file: Vec<_> = file_menu_actions(false, false)
            .into_iter()
            .flatten()
            .collect();
        let dir: Vec<_> = file_menu_actions(true, false)
            .into_iter()
            .flatten()
            .collect();
        assert!(file.contains(&OpenWithDefault));
        assert!(!dir.contains(&OpenWithDefault));
        assert!(dir.contains(&NewFile) && dir.contains(&NewFolder));
        assert!(!file.contains(&NewFile) && !file.contains(&NewFolder));
        // 有状态的目录同样不给 ViewDiff
        let dirty_dir: Vec<_> = file_menu_actions(true, true)
            .into_iter()
            .flatten()
            .collect();
        assert!(!dirty_dir.contains(&ViewDiff));
        assert_eq!(dirty_dir, dir);
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
                vec![entry("node_modules", "/p/only-ignored/node_modules", true, true)],
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
}

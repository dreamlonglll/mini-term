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
use gpui_component::tooltip::Tooltip;
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
                        // 原版第二条:`fs-change` 且属于当前项目 → 500ms 去抖刷 git 状态。
                        // watcher 是按项目根注册的,能走到这儿的必然属于当前项目。
                        tree.schedule_git_refresh();
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
        self.chain_owner.clear();
        self.git_status.clear();
        self.git_refresh_at = None;
        self.root_error = None;
        self.current_project = project_id;
        // 没有项目时把旁路那一份关掉:reader 线程上那道总闸能少开一个人是一个
        git_watch::set_enabled_for(git_watch::Subscriber::FileTree, root.is_some());
        if let Some(root) = root {
            self.root_loading = true;
            self.load_dir(root.clone(), root, cx);
            // 原版第一条:切项目时与 `list_directory` 并发拉一次
            self.load_git_status(cx);
        } else {
            self.root_loading = false;
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
        // 根目录那一趟额外承担三态占位(loading / 加载失败 / 刷新失败)
        let is_root = dir == root;
        cx.spawn(async move |this, cx| {
            // list_directory 是阻塞 IO(还要逐级读 .gitignore),必须离开主线程;
            // 单链压缩最多再串行列 7 层,**整段都在后台**跑完再回来
            let result = cx
                .background_executor()
                .spawn(async move {
                    let entries = mt_project::fs::list_directory(&task_root, &task_dir)?;
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
                        tree.entries.insert(dir, entries);
                    }
                    Err(err) => {
                        eprintln!("[files] 列目录失败: {err:#}");
                        if is_root {
                            tree.root_error = Some(format!("{err:#}"));
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
        let Some(root) = self.project_root(cx) else {
            return;
        };
        // 没有内容可显示时才亮 loading —— 有旧内容就静默重列(原版同一条口径)
        if !self.entries.contains_key(&root) {
            self.root_loading = true;
        }
        self.load_dir(root.clone(), root, cx);
        self.load_git_status(cx);
        cx.notify();
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
            });
            if expanded {
                self.rows(project_id, root, &entry.path, depth + 1, cx, out);
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
    /// 相对项目根、`/` 分隔的路径(git 状态表的键,「查看变更」也拿它当参数)。
    rel: String,
    /// git 状态字母 + **是不是汇总来的**(汇总的那枚画淡一档)。
    git: Option<(String, bool)>,
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
                            .tooltip(|window, cx| {
                                Tooltip::new(t("fileTree", "header.refresh")).build(window, cx)
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

        let mut rows = Vec::new();
        self.rows(&project_id, &root, &root, 0, cx, &mut rows);

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

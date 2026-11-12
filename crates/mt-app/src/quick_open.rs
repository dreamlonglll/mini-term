//! 快速打开文件(Ctrl+Shift+O),对标 VS Code 的 Ctrl+P —— 只做「按文件名跳文件」,
//! `>` 命令面板与 `@` 符号列表不做。后端是 [`mt_project::quick_open`]。
//!
//! 面板的形态(最近打开排前、`文件:行:列`、只输 `:行` 时在当前文件里跳行)移植自
//! Winc(Apache-2.0)`crates/winc-app/src/app/workspace/picker.rs`;版面、开合与键盘口径
//! 照本仓的项目切换器([`crate::project_switcher`])。
//!
//! # 与全局搜索的分工
//!
//! 全局搜索(Ctrl+Shift+F,[`crate::search_modal`])的「文件名」模式是子串匹配、按 Enter 才
//! 遍历一次磁盘、结果按路径排序,一个字没动。这里是独立入口:每个项目建一份内存索引,输入框
//! **边打字边出结果**(模糊打分,文件名命中 / 词首 / 连续命中优先),两边的忽略规则共用同一个
//! walker,能搜到的文件集合一致。
//!
//! # 索引:缓存、失效与线程
//!
//! - **每个项目缓存一份**([`CachedIndex`],最多 [`MAX_CACHED_PROJECTS`] 个,按最近使用淘汰)。
//!   打开面板时立刻拿缓存出结果;缓存过期(距上次建好超过 [`refresh_interval`]:至少
//!   [`REFRESH_AFTER`],建一次越慢的项目隔得越久)或被标脏就在后台重建,建好后**整份替换**、
//!   面板开着就按新表重算并保住选中的那一行 —— 新表落地之前旧表照用,面板不会变空。第一次
//!   打开时没有缓存,列表显示「正在建立文件索引…」。
//! - **标脏**接的是文件树的目录监听([`note_fs_change`]):有文件 / 目录出现或消失就标脏。文件树
//!   只盯**展开着的**目录,折起来的目录里、终端里 AI 新建的文件它听不到 —— 所以另有过期兜底,
//!   最坏也就是打开面板那一刻少几个刚出现的文件,后台重建完自动补上。没有另挂一路覆盖整个项目
//!   的递归监听:那要连 `node_modules` / `target` 的洪水一起收(Git 面板的工作区监听为此专门
//!   拆过子树,见 `git_dir_watch.rs`),只为一个按需打开的面板不值得。
//! - 建索引要走一遍整棵目录树(WSL / 网络盘上可能几秒),走**专用线程**(`std::thread`)、结果经
//!   oneshot 交回主线程。不用 `background_executor().spawn`:理由同 `search_modal` 模块注释 ——
//!   跑几秒的同步闭包会占死执行器的一根工作线程,文件树列目录、外部编辑器拉起都在那个池子里排队。
//! - 上限:文件数 [`mt_project::quick_open::MAX_FILES`]、耗时
//!   [`mt_project::quick_open::BUILD_TIMEOUT`],超了截断,底栏标「结果不完整」。
//!
//! # 打分
//!
//! 索引不超过 [`SYNC_SCORE_MAX`] 条时在主线程上同步打分(一次按键亚毫秒级,结果与按键同帧
//! 出来);再大就每次按键起一条后台线程打分。新的查询一来,旧线程的取消标志置位、旧的前台任务
//! 整个换掉(`score_generation` 再挡一层),过期结果到不了界面;新结果回来之前旧结果照显,输入不卡。
//! 命中字符的下标只对**画出来的那几行**现算(虚拟列表的回调里),不在打分的耗时路径上。
//!
//! # 最近打开
//!
//! 记账在文件页签那条统一的打开路径上(`workbench_area::open_document` / 点页签切过去),每个
//! 项目一份、只放内存(见 `workbench_area::ProjectDocuments::recent`)。空查询时它们排在最前
//! (**不含**当前正停着的那个文件:打开面板直接回车就切回上一个文件);有查询时同分靠前。
//! 不在索引里的(已删除 / 被忽略规则挡掉的)不列。
//!
//! # 远程项目与没有项目
//!
//! SSH 远程项目不支持:远端没有可用的批量遍历(文件树是逐级 SFTP readdir,列全量是成百上千次
//! 往返),全局搜索那边同样不支持。面板照常打开,列表区直接说明原因,不是一张空列表;没有活动
//! 项目时同理。
//!
//! # 键盘
//!
//! ↑ / ↓ 绑在 `"QuickOpen > Input"` 上(与项目切换器同一个理由:谓词与 `Input` 同深度才压得过
//! 输入框自带的方向键,见 `project_switcher` 模块注释);Enter 订阅输入框的 `PressEnter`;Esc 与
//! 点遮罩是 Dialog 自带的。打开文件**先关窗再打开**:`close_dialog` 会把焦点还给打开面板之前的
//! 那个元素,文件页的聚焦排在它后面才落得到编辑器上。
//!
//! 列号目前只解析不定位:文件页的定位([`LineTarget`])只到行。跳到的行滚到视口正中
//! ([`LineTarget::centered`],对标 IDEA 的 Ctrl+G);终端文件链接、全局搜索仍是顶到视口顶上。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Div, Entity, FontWeight, Global,
    HighlightStyle, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, Render,
    ScrollStrategy, SharedString, Size, StatefulInteractiveElement, Styled, StyledText,
    Subscription, Task, Window, actions, div, prelude::FluentBuilder, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use mt_project::quick_open::{
    BuildLimits, FileIndex, Goto, Incomplete, MAX_FILES, SearchOutcome, normalize_query,
    relative_query, split_goto,
};
use mt_ui::icons::FileIcon;

use crate::file_viewer::LineTarget;
use crate::i18n::{t, tr};
use crate::overlay::kind;
use crate::project_switcher::hint;
use crate::prompt::{autofocus, close_guarded, open_guarded};
use crate::store::{AppStore, StoreEvent};
use crate::ui;

actions!(
    mini_term,
    [
        /// 快速打开:上一个候选(↑)。绑在 `"QuickOpen > Input"` 上,见模块注释「键盘」。
        QuickOpenPrev,
        /// 快速打开:下一个候选(↓)。
        QuickOpenNext,
    ]
);

/// 面板的 key context(方向键绑定的谓词是 `"QuickOpen > Input"`,见 `hotkeys.rs`)。
pub const CONTEXT: &str = "QuickOpen";

/// 结果列表最多显示几条(Winc 同值)。再往后用户也不会去翻,应当多打两个字。
const MAX_RESULTS: usize = 200;

/// 索引建好多久之后,再次打开面板时在后台重建一次(见模块注释「索引」)。这是下限:建一次要
/// 花得久的大项目按 [`refresh_interval`] 拉长,免得每开一次面板就把整棵树重走一遍。
const REFRESH_AFTER: Duration = Duration::from_secs(10);

/// 过期间隔的上限。被标脏不受它管,随时重建。
const MAX_REFRESH_AFTER: Duration = Duration::from_secs(300);

/// 过期间隔 = 上次建索引耗时的这么多倍(夹在 [`REFRESH_AFTER`]..=[`MAX_REFRESH_AFTER`])。
/// 50ms 建好的小项目每 10 秒可刷一次;2 秒的大仓库 40 秒一次;建到超时(10 秒)的 200 秒一次
/// —— 后台遍历占的时间始终不超过总时长的二十分之一。
const REFRESH_COST_FACTOR: u32 = 20;

/// 文件数不超过这个数的索引在主线程上同步打分。五千条路径打一遍分是亚毫秒级。
const SYNC_SCORE_MAX: usize = 5_000;

/// 最多同时缓存几个项目的索引。二十万个文件的索引约十几 MB,来回切的项目通常也就两三个。
const MAX_CACHED_PROJECTS: usize = 3;

struct GlobalQuickOpen(Entity<QuickOpenPanel>);
impl Global for GlobalQuickOpen {}

/// 面板眼下对着谁。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// 没有活动项目。
    NoProject,
    /// 活动项目是 SSH 远程项目(不支持,见模块注释)。
    Remote,
    /// 本地项目(含 WSL 路径的项目:它们在本机文件系统里可遍历)。
    Local { project_id: String, root: PathBuf },
}

/// 一个项目的索引缓存。
struct CachedIndex {
    root: PathBuf,
    index: Option<Arc<FileIndex>>,
    built_at: Option<Instant>,
    /// 在跑的那次重建是什么时候起的(算耗时用)。
    started_at: Option<Instant>,
    /// 上次建索引花了多久,决定过期间隔([`refresh_interval`])。
    last_build_took: Duration,
    /// 文件树报过「有文件出现 / 消失」,下次打开面板时重建。**开始**重建时清掉 —— 重建期间
    /// 又来的变化会重新标上,不会被这一趟吞掉。
    stale: bool,
    /// 在跑的那次重建的取消标志。
    building: Option<Arc<AtomicBool>>,
    /// 在跑的那次重建的代次(取自面板全局递增的 [`QuickOpenPanel::build_counter`]);回来的结果
    /// 代次对不上就丢(被后来者顶掉的那次)。不按条目各自从 0 数:条目被换根 / 淘汰后重建,
    /// 新旧两趟的代次会撞上,旧根的表就会落进新条目。
    generation: u64,
    last_used: Instant,
}

impl CachedIndex {
    fn new(root: PathBuf, now: Instant) -> Self {
        Self {
            root,
            index: None,
            built_at: None,
            started_at: None,
            last_build_took: Duration::ZERO,
            stale: false,
            building: None,
            generation: 0,
            last_used: now,
        }
    }
}

impl Drop for CachedIndex {
    /// 被淘汰 / 换根时,还在跑的遍历线程跟着停。
    fn drop(&mut self) {
        if let Some(cancel) = self.building.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// 正在显示的一批结果。`hits` 是 `index` 的下标,**两者同生同死**:索引换了一份新的,下标
/// 就对不上了,所以结果总是连同产出它的那份索引一起存。
struct Shown {
    index: Arc<FileIndex>,
    needle: Rc<[char]>,
    hits: Vec<usize>,
    /// `hits` 里哪些是最近打开过的(行尾挂个小标签)。
    recent: HashSet<usize>,
    /// 截断前一共命中几个(空查询时是文件总数)。
    matched: usize,
}

pub struct QuickOpenPanel {
    store: Entity<AppStore>,
    query: Entity<InputState>,
    target: Target,
    indexes: HashMap<String, CachedIndex>,
    shown: Option<Shown>,
    /// 查询末尾的 `:行[:列]`。
    goto: Option<Goto>,
    /// 查询只有 `:行`(文件段为空):Enter 在当前文件页里跳行。
    goto_only: bool,
    cursor: usize,
    /// 结果虚拟列表的滚动位置。必须存在视图上,理由同 `search_modal::SearchModal::results_scroll`。
    scroll: VirtualListScrollHandle,
    /// 每起一次建索引加一,见 [`CachedIndex::generation`]。
    build_counter: u64,
    /// 每次重算加一;后台打分回来时代次对不上就丢。
    score_generation: u64,
    score_cancel: Option<Arc<AtomicBool>>,
    _score_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl Drop for QuickOpenPanel {
    fn drop(&mut self) {
        if let Some(cancel) = self.score_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn overlay_open() -> bool {
    crate::overlay::contains(crate::overlay::key(kind::QUICK_OPEN))
}

/// 打开面板。已经开着时是空操作(toggle 的「关」在 `main.rs` 的处理器里)。
pub fn open(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    // 守卫在**建视图之前**判:`open_guarded` 拦下来时输入框已建好、聚焦也排上了,焦点会被
    // 送进虚空(与 `prompt::show_prompt` 同一个坑)
    if overlay_open() {
        return;
    }
    let view = if let Some(global) = cx.try_global::<GlobalQuickOpen>() {
        global.0.clone()
    } else {
        let view = cx.new(|cx| QuickOpenPanel::new(store, window, cx));
        cx.set_global(GlobalQuickOpen(view.clone()));
        view
    };
    view.update(cx, |view, cx| view.begin(window, cx));
    let input = view.read(cx).query.clone();

    open_guarded(kind::QUICK_OPEN, window, cx, {
        let view = view.clone();
        move |dialog, window, _cx| {
            let viewport = window.viewport_size();
            let width = (viewport.width * 0.8).min(px(680.0));
            // 固定高度:结果条数随按键变,高度跟着跳会晃眼
            let height = (viewport.height * 0.6).min(px(560.0));
            dialog
                // 头 / 尾都是自己画的分隔线,交给 Dialog 加边距会把线切断
                .p_0()
                // 与项目切换器一样不要右上角的 ✕(`p_0()` 之下它会压在输入框上)
                .close_button(false)
                .w(width)
                .child(div().h(height).child(view.clone()))
        }
    });

    // Dialog 打开时会把焦点抢到自己面板上,聚焦输入框必须排在它后面(见 `prompt::autofocus`)
    autofocus(&input, window, cx);
}

/// 文件树的目录监听报上来一条变化。有文件 / 目录出现或消失(`structural`)就把包含它的
/// 项目索引标脏,下次打开面板时重建(见模块注释「索引」)。
pub fn note_fs_change(path: &Path, structural: bool, cx: &mut App) {
    if !structural {
        return;
    }
    let Some(view) = cx.try_global::<GlobalQuickOpen>().map(|g| g.0.clone()) else {
        return;
    };
    view.update(cx, |view, _cx| view.mark_stale(path));
}

impl QuickOpenPanel {
    fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("search", "quickOpen.placeholder"))
        });
        let input_sub = cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                // 边打字边出结果,不必按 Enter
                InputEvent::Change => this.rescore(false, cx),
                InputEvent::PressEnter { .. } => this.accept(window, cx),
                _ => {}
            },
        );
        // 面板开着时活动项目换了 / 被删了 / 变成远程的:换目标重算。关着时不管,下次打开
        // 自会重新对齐([`Self::begin`])
        let store_sub = cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
            if !event.touches_active_project() || !overlay_open() {
                return;
            }
            if this.sync_target(cx) {
                this.ensure_index(cx);
                this.rescore(false, cx);
            }
        });
        Self {
            store,
            query,
            target: Target::NoProject,
            indexes: HashMap::new(),
            shown: None,
            goto: None,
            goto_only: false,
            cursor: 0,
            scroll: VirtualListScrollHandle::new(),
            build_counter: 0,
            score_generation: 0,
            score_cancel: None,
            _score_task: None,
            _subs: vec![input_sub, store_sub],
        }
    }

    /// 每次打开面板:清空输入、对齐活动项目、按需重建索引、列出空查询的结果。
    fn begin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // `set_value` 不发 Change 事件,结果在下面自己重算
        self.query
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.sync_target(cx);
        self.ensure_index(cx);
        self.rescore(false, cx);
    }

    /// 按活动项目重算目标;变了就把手上的结果作废。返回是否变了。
    fn sync_target(&mut self, cx: &mut Context<Self>) -> bool {
        let target = {
            let store = self.store.read(cx);
            match store.active_project() {
                None => Target::NoProject,
                Some(project) if store.is_remote_project(&project.id) => Target::Remote,
                Some(project) => Target::Local {
                    project_id: project.id.clone(),
                    root: PathBuf::from(&project.path),
                },
            }
        };
        if target == self.target {
            return false;
        }
        self.target = target;
        self.shown = None;
        self.cursor = 0;
        true
    }

    /// 当前项目的索引:没有就建,过期 / 标脏就在后台重建(旧表照用),顺带淘汰多余的缓存。
    fn ensure_index(&mut self, cx: &mut Context<Self>) {
        let Target::Local { project_id, root } = self.target.clone() else {
            return;
        };
        let now = Instant::now();
        let entry = self
            .indexes
            .entry(project_id.clone())
            .or_insert_with(|| CachedIndex::new(root.clone(), now));
        // 项目路径被改过:旧表是另一个目录的,整份作废(drop 时顺带停掉在跑的遍历)
        if entry.root != root {
            *entry = CachedIndex::new(root.clone(), now);
        }
        entry.last_used = now;
        let due = needs_rebuild(
            entry.index.is_some(),
            entry.stale,
            entry.built_at,
            entry.building.is_some(),
            refresh_interval(entry.last_build_took),
            now,
        );
        if due {
            self.start_build(project_id.clone(), root, cx);
        }
        let ages = self
            .indexes
            .iter()
            .map(|(id, entry)| (id.clone(), entry.last_used))
            .collect::<Vec<_>>();
        if let Some(victim) = eviction_victim(&ages, &project_id, MAX_CACHED_PROJECTS) {
            self.indexes.remove(&victim);
        }
    }

    /// 在专用线程上建索引,建好经 oneshot 交回主线程(理由见模块注释「索引」)。
    fn start_build(&mut self, project_id: String, root: PathBuf, cx: &mut Context<Self>) {
        self.build_counter = self.build_counter.wrapping_add(1);
        let generation = self.build_counter;
        let Some(entry) = self.indexes.get_mut(&project_id) else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = oneshot::channel::<Option<FileIndex>>();
        let worker_cancel = cancel.clone();
        let worker_root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("mt-quick-open-index".into())
            .spawn(move || {
                let index = FileIndex::build(&worker_root, BuildLimits::default(), &worker_cancel);
                let _ = tx.send(index);
            });
        if let Err(err) = spawned {
            eprintln!("[quick-open] 建索引线程起不来: {err}");
            return;
        }
        if let Some(previous) = entry.building.replace(cancel) {
            previous.store(true, Ordering::Relaxed);
        }
        entry.stale = false;
        entry.generation = generation;
        entry.started_at = Some(Instant::now());
        // 不把 Task 存进缓存条目:落地时要清掉 `building`,在任务自己的回调里丢掉自己的
        // Task 是自找麻烦。被顶掉的那次靠取消标志收工、靠代次丢结果
        cx.spawn(async move |this, cx| {
            let Ok(Some(index)) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this: &mut QuickOpenPanel, cx| {
                this.on_index_built(&project_id, generation, index, cx);
            });
        })
        .detach();
    }

    fn on_index_built(
        &mut self,
        project_id: &str,
        generation: u64,
        index: FileIndex,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.indexes.get_mut(project_id) else {
            return;
        };
        // 代次对上了还要核一下根:防的是「换根之后」这种代次以外的错配
        if entry.generation != generation || index.root() != entry.root {
            return;
        }
        let now = Instant::now();
        entry.building = None;
        entry.index = Some(Arc::new(index));
        entry.built_at = Some(now);
        if let Some(started) = entry.started_at.take() {
            entry.last_build_took = now.saturating_duration_since(started);
        }
        // 面板开着、对着的正是这个项目:按新表重算,保住选中的那一行(用户可能已经按 ↓ 选好、
        // 正要回车)
        let current =
            matches!(&self.target, Target::Local { project_id: id, .. } if id == project_id);
        if current && overlay_open() {
            self.rescore(true, cx);
        }
        cx.notify();
    }

    /// 文件树报上来的结构变化落在哪个缓存的项目根下,就把它标脏。
    fn mark_stale(&mut self, path: &Path) {
        for entry in self.indexes.values_mut() {
            if path.starts_with(&entry.root) {
                entry.stale = true;
            }
        }
    }

    /// 按输入框里的查询重算结果。`preserve` = 索引换了新的、查询没变:尽量保住选中的那一行;
    /// 否则(查询变了)游标回到第一条。
    fn rescore(&mut self, preserve: bool, cx: &mut Context<Self>) {
        self.score_generation = self.score_generation.wrapping_add(1);
        if let Some(cancel) = self.score_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self._score_task = None;

        let raw = self.query.read(cx).value().to_string();
        let (file_part, goto) = split_goto(&raw);
        self.goto = goto;
        self.goto_only = goto.is_some() && file_part.trim().is_empty();
        let Target::Local { project_id, root } = &self.target else {
            self.shown = None;
            cx.notify();
            return;
        };
        if self.goto_only {
            // 只输了 `:行`:不列文件,Enter 在当前文件里跳行
            self.shown = None;
            self.cursor = 0;
            cx.notify();
            return;
        }
        let Some(index) = self
            .indexes
            .get(project_id)
            .and_then(|entry| entry.index.clone())
        else {
            // 第一次建索引还没回来;建好时 `on_index_built` 会再来一趟
            self.shown = None;
            cx.notify();
            return;
        };

        let needle: Rc<[char]> = normalize_query(&relative_query(root, file_part)).into();
        let recent = recent_positions(&index, project_id, needle.is_empty(), cx);
        let keep = preserve.then(|| self.selected_path()).flatten();

        if needle.is_empty() || index.len() <= SYNC_SCORE_MAX {
            let outcome = index
                .search(&needle, &recent, MAX_RESULTS, None)
                .unwrap_or_else(empty_outcome);
            self.apply(index, needle, recent, outcome, keep);
            cx.notify();
            return;
        }

        // 大索引:整次打分挪到一条后台线程,旧结果照显到新结果回来
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = oneshot::channel::<Option<SearchOutcome>>();
        let spawned = {
            let (index, needle, recent, cancel) = (
                index.clone(),
                needle.to_vec(),
                recent.clone(),
                cancel.clone(),
            );
            std::thread::Builder::new()
                .name("mt-quick-open-score".into())
                .spawn(move || {
                    let _ = tx.send(index.search(&needle, &recent, MAX_RESULTS, Some(&cancel)));
                })
        };
        if let Err(err) = spawned {
            // 线程起不来就当场算:卡一下总比没结果好
            eprintln!("[quick-open] 打分线程起不来: {err}");
            let outcome = index
                .search(&needle, &recent, MAX_RESULTS, None)
                .unwrap_or_else(empty_outcome);
            self.apply(index, needle, recent, outcome, keep);
            cx.notify();
            return;
        }
        self.score_cancel = Some(cancel);
        let generation = self.score_generation;
        self._score_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Some(outcome)) = rx.await else {
                return;
            };
            let _ = this.update(cx, |this: &mut QuickOpenPanel, cx| {
                if this.score_generation != generation {
                    return;
                }
                this.score_cancel = None;
                this.apply(index, needle, recent, outcome, keep);
                cx.notify();
            });
        }));
    }

    fn apply(
        &mut self,
        index: Arc<FileIndex>,
        needle: Rc<[char]>,
        recent: Vec<usize>,
        outcome: SearchOutcome,
        keep: Option<String>,
    ) {
        let kept = keep.and_then(|path| {
            outcome
                .hits
                .iter()
                .position(|&hit| index.path(hit) == Some(path.as_str()))
        });
        self.shown = Some(Shown {
            index,
            needle,
            hits: outcome.hits,
            recent: recent.into_iter().collect(),
            matched: outcome.matched,
        });
        match kept {
            Some(row) => {
                self.cursor = row;
                self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
            }
            None => {
                self.cursor = 0;
                self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
        }
    }

    /// 游标所在那一行的相对路径。
    fn selected_path(&self) -> Option<String> {
        let shown = self.shown.as_ref()?;
        let &hit = shown.hits.get(self.cursor)?;
        shown.index.path(hit).map(str::to_string)
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = self.shown.as_ref().map_or(0, |shown| shown.hits.len());
        self.cursor = crate::project_switcher::next_cursor(self.cursor, len, delta);
        self.scroll
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter / 单击:打开游标所在的文件(带 `:行` 就定位过去);只输了 `:行` 时在当前文件页里
    /// 跳行。先关窗再打开,理由见模块注释「键盘」。
    fn accept(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Local { project_id, .. } = self.target.clone() else {
            return;
        };
        let line = self.goto.map(|goto| LineTarget::line(goto.line).centered());
        let path = if self.goto_only {
            crate::workbench_area::active_document_path(&project_id, cx)
        } else {
            self.shown.as_ref().and_then(|shown| {
                let row = self.cursor.min(shown.hits.len().saturating_sub(1));
                shown
                    .hits
                    .get(row)
                    .and_then(|&hit| shown.index.absolute(hit))
            })
        };
        let Some(path) = path else {
            return;
        };
        let store = self.store.clone();
        // 关窗排到本轮 effect 之后:这会儿本实体正被 Dialog 的 builder 持有着
        window.defer(cx, move |window, cx| {
            close_guarded(kind::QUICK_OPEN, window, cx);
            // 这一拍之间活动项目换了就不开:路径是按原项目根拼的,`open_active_file` 却按
            // 当下的活动项目认来源
            if store.read(cx).active_project_id.as_deref() != Some(project_id.as_str()) {
                return;
            }
            crate::workbench_area::open_active_file(store, path, line, window, cx);
        });
    }
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 缓存要不要在后台重建:没在建,且(还没有表 / 被标脏 / 建好超过 `refresh_after`)。
fn needs_rebuild(
    has_index: bool,
    stale: bool,
    built_at: Option<Instant>,
    building: bool,
    refresh_after: Duration,
    now: Instant,
) -> bool {
    !building
        && (!has_index
            || stale
            || built_at.is_none_or(|at| now.saturating_duration_since(at) >= refresh_after))
}

/// 过期间隔:上次建索引耗时的 [`REFRESH_COST_FACTOR`] 倍,夹在
/// [`REFRESH_AFTER`]..=[`MAX_REFRESH_AFTER`]。
fn refresh_interval(last_build_took: Duration) -> Duration {
    last_build_took
        .saturating_mul(REFRESH_COST_FACTOR)
        .clamp(REFRESH_AFTER, MAX_REFRESH_AFTER)
}

/// 缓存超过 `cap` 个时该淘汰谁:除 `keep`(当前项目)之外最久没用的那个。
fn eviction_victim(entries: &[(String, Instant)], keep: &str, cap: usize) -> Option<String> {
    if entries.len() <= cap {
        return None;
    }
    entries
        .iter()
        .filter(|(id, _)| id != keep)
        .min_by_key(|(_, used)| *used)
        .map(|(id, _)| id.clone())
}

/// 「最近打开」在这份索引里的下标,新的在前。空查询时去掉当前停着的那个文件(见模块注释
/// 「最近打开」);不在索引里的丢掉。
fn recent_positions(
    index: &FileIndex,
    project_id: &str,
    exclude_active: bool,
    cx: &App,
) -> Vec<usize> {
    let active = exclude_active
        .then(|| crate::workbench_area::active_document_path(project_id, cx))
        .flatten()
        .and_then(|path| index.position_of(&path));
    crate::workbench_area::recent_files(project_id, cx)
        .iter()
        .filter_map(|path| index.position_of(path))
        .filter(|&position| Some(position) != active)
        .collect()
}

fn empty_outcome() -> SearchOutcome {
    SearchOutcome {
        hits: Vec::new(),
        matched: 0,
    }
}

/// 把排好序的命中字符下标合并成 `[起, 止)` 区间(相邻的并成一段)。
fn char_runs(positions: &[usize]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &pos in positions {
        match runs.last_mut() {
            Some((_, end)) if *end == pos => *end += 1,
            _ => runs.push((pos, pos + 1)),
        }
    }
    runs
}

/// 一行要画的两段:文件名与所在目录(不带末尾的 `/`,根目录下的文件为空串),各自带上落在
/// 本段的命中字符下标(已换算成段内下标)。`positions` 是整条相对路径上的字符下标。
fn split_row<'a>(
    path: &'a str,
    name: &'a str,
    positions: &[usize],
) -> (&'a str, Vec<usize>, &'a str, Vec<usize>) {
    let name_start = path.chars().count() - name.chars().count();
    let dir = path[..path.len() - name.len()].trim_end_matches('/');
    let dir_chars = dir.chars().count();
    let name_hits = positions
        .iter()
        .filter(|&&pos| pos >= name_start)
        .map(|&pos| pos - name_start)
        .collect();
    let dir_hits = positions
        .iter()
        .copied()
        .filter(|&pos| pos < dir_chars)
        .collect();
    (name, name_hits, dir, dir_hits)
}

/// 底栏右侧那几句状态(不完整的那句用警示色)。
fn status_items(
    shown: Option<(usize, usize, bool)>,
    refreshing: bool,
    incomplete: Option<Incomplete>,
    goto: Option<Goto>,
    goto_only: bool,
) -> Vec<(String, bool)> {
    let mut items = Vec::new();
    if let Some((matched, total, has_query)) = shown {
        items.push((
            if has_query {
                tr!("search", "quickOpen.matchCount", count = matched)
            } else {
                tr!("search", "quickOpen.fileCount", count = total)
            },
            false,
        ));
    }
    if refreshing {
        items.push((t("search", "quickOpen.refreshing").to_string(), false));
    }
    match incomplete {
        Some(Incomplete::TooManyFiles) => items.push((
            tr!("search", "quickOpen.incompleteTooMany", count = MAX_FILES),
            true,
        )),
        Some(Incomplete::TimedOut) => {
            items.push((t("search", "quickOpen.incompleteTimeout").to_string(), true))
        }
        None => {}
    }
    if let Some(goto) = goto
        && !goto_only
    {
        items.push((tr!("search", "quickOpen.gotoLine", line = goto.line), false));
    }
    items
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 带命中高亮的一段文字,放不下时出省略号。命中处 accent + 半粗,与项目切换器同一观感。
fn highlighted_label(text: &str, hits: &[usize], size: f32, base: Hsla) -> Div {
    let style = HighlightStyle {
        color: Some(ui::accent()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let highlights: Vec<_> = char_runs(hits)
        .into_iter()
        .filter_map(|(start, end)| crate::git_branches::char_range_to_bytes(text, start, end))
        .map(|range| (range, style))
        .collect();
    div()
        .min_w_0()
        .truncate()
        .text_size(ui::font_px(size))
        .text_color(base)
        .child(StyledText::new(text.to_string()).with_highlights(highlights))
}

/// 结果行的高度。虚拟列表要在不建元素的前提下拿到行高,所以行高钉成「上下留白 + 一行文字」,
/// 一行文字的高度按窗口当下的口径现算 —— 与 `search_modal::RowMetrics` 同一个算法,换 UI 字号 /
/// 换 DPI 都不用改常量。行内最大字号是文件名的 12。
fn row_height(window: &Window) -> Pixels {
    let style = window.text_style();
    let rem = window.rem_size();
    window.pixel_snap(style.line_height.to_pixels(ui::font_px(12.0).into(), rem)) + px(16.0)
}

impl QuickOpenPanel {
    /// 列表区没有结果可画时的那句话;有结果时为 `None`。
    fn empty_hint(&self, cx: &App) -> Option<SharedString> {
        let text: SharedString = match &self.target {
            Target::NoProject => t("search", "quickOpen.noProject").into(),
            Target::Remote => t("search", "quickOpen.remoteUnsupported").into(),
            Target::Local { project_id, .. } => {
                if self.goto_only {
                    let line = self.goto.map_or(1, |goto| goto.line);
                    return Some(
                        if crate::workbench_area::active_document_path(project_id, cx).is_some() {
                            tr!("search", "quickOpen.gotoInCurrent", line = line).into()
                        } else {
                            t("search", "quickOpen.gotoNoDocument").into()
                        },
                    );
                }
                let shown = self.shown.as_ref()?;
                if shown.index.is_empty() {
                    t("search", "quickOpen.empty").into()
                } else if shown.hits.is_empty() {
                    t("search", "quickOpen.noMatch").into()
                } else {
                    return None;
                }
            }
        };
        Some(text)
    }

    /// ⚠️ 行根必须 `w_full` + 钉死 `h`(与 `search_modal::render_result_row` 同理):虚拟列表把
    /// 每一行当根元素单量,不给 `w_full` 时底色只盖住文字那一截,高度对不上则裁字或留缝。
    fn render_row(&self, row: usize, h: Pixels, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.shown.as_ref()?;
        let &hit = shown.hits.get(row)?;
        let file = shown.index.file(hit)?;
        let positions = shown.index.matched_positions(&shown.needle, hit);
        let (name, name_hits, dir, dir_hits) = split_row(file.path, file.name, &positions);
        let is_cursor = row == self.cursor;
        let is_recent = shown.recent.contains(&hit);

        Some(
            div()
                .id(SharedString::from(format!("quick-open-row-{row}")))
                .flex()
                .items_center()
                .gap(px(8.0))
                .w_full()
                .h(h)
                .px(px(16.0))
                .overflow_hidden()
                .cursor_pointer()
                .when(is_cursor, |el| el.bg(ui::accent_subtle()))
                .when(!is_cursor, |el| el.hover(|el| el.bg(ui::border_subtle())))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.cursor = row;
                    this.accept(window, cx);
                }))
                .child(
                    div()
                        .flex_none()
                        .child(FileIcon::new(name, false, false).size(px(16.0))),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(
                            highlighted_label(name, &name_hits, 12.0, ui::text_primary())
                                .flex_none()
                                .max_w(gpui::relative(0.7)),
                        )
                        .when(!dir.is_empty(), |el| {
                            el.child(
                                highlighted_label(dir, &dir_hits, 10.0, ui::text_muted()).flex_1(),
                            )
                        }),
                )
                .when(is_recent, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(ui::font_px(10.0))
                            .text_color(ui::text_muted())
                            .child(t("search", "quickOpen.recent")),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(message) = self.empty_hint(cx) {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.0))
                .text_center()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(message)
                .into_any_element();
        }
        let Some(shown) = self.shown.as_ref() else {
            // 本地项目、第一次建索引还没回来
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(t("search", "quickOpen.indexing"))
                .into_any_element();
        };
        let h = row_height(window);
        let sizes: Rc<Vec<Size<Pixels>>> = Rc::new(vec![size(px(0.0), h); shown.hits.len()]);
        v_virtual_list(
            cx.entity(),
            "quick-open-results",
            sizes,
            move |this, range, _window, cx| {
                range
                    .filter_map(|row| this.render_row(row, h, cx))
                    .collect()
            },
        )
        .track_scroll(&self.scroll)
        // 横向不滚:组件库给 base 挂的是双轴滚动,路径长的那行会凭空多出横向可滚范围
        .overflow_x_hidden()
        .into_any_element()
    }

    fn render_footer(&self) -> impl IntoElement {
        let entry = match &self.target {
            Target::Local { project_id, .. } => self.indexes.get(project_id),
            _ => None,
        };
        let shown = self
            .shown
            .as_ref()
            .map(|shown| (shown.matched, shown.index.len(), !shown.needle.is_empty()));
        let refreshing =
            self.shown.is_some() && entry.is_some_and(|entry| entry.building.is_some());
        let incomplete = self
            .shown
            .as_ref()
            .and_then(|shown| shown.index.incomplete());
        let items = status_items(shown, refreshing, incomplete, self.goto, self.goto_only);

        let mut status = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_w_0()
            .overflow_hidden()
            .text_size(ui::font_px(10.0));
        for (index, (text, warn)) in items.into_iter().enumerate() {
            if index > 0 {
                status = status.child(div().flex_none().text_color(ui::text_muted()).child("·"));
            }
            status = status.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(if warn {
                        ui::color_warning()
                    } else {
                        ui::text_muted()
                    })
                    .child(text),
            );
        }

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .flex_none()
            .px(px(12.0))
            .py(px(8.0))
            .border_t_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .flex_none()
                    .child(hint(&["↑", "↓"], t("projectSwitcher", "hintMove")))
                    .child(hint(&["Enter"], t("projectSwitcher", "hintOpen")))
                    .child(hint(&["Esc"], t("projectSwitcher", "hintClose"))),
            )
            .child(status)
    }
}

impl Render for QuickOpenPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 虚拟列表自带 `size_full`,撑开那一格交给外面这层 `flex_1`;`overflow_hidden` 顺带把
        // flex 项的自动最小高度压成 0,不然两百行的内容高会把这一格顶出弹窗
        let list = div()
            .flex_1()
            .overflow_hidden()
            .py(px(4.0))
            .child(self.render_list(window, cx));
        div()
            // 方向键的上下文锚点,谓词是 `"QuickOpen > Input"`(见模块注释「键盘」)
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &QuickOpenPrev, _window, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &QuickOpenNext, _window, cx| {
                this.move_cursor(1, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(Input::new(&self.query).cleanable(false)),
            )
            .child(list)
            .child(self.render_footer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyBinding, KeyContext, Keymap, Keystroke};

    #[test]
    fn 没表_标脏_过期才重建_在建时不重复起() {
        let now = Instant::now();
        let fresh = Some(now);
        let every = REFRESH_AFTER;
        assert!(
            needs_rebuild(false, false, None, false, every, now),
            "还没有表"
        );
        assert!(
            !needs_rebuild(true, false, fresh, false, every, now),
            "刚建好不重建"
        );
        assert!(
            needs_rebuild(true, true, fresh, false, every, now),
            "被标脏"
        );
        let old = now.checked_sub(REFRESH_AFTER + Duration::from_secs(1));
        if old.is_some() {
            assert!(needs_rebuild(true, false, old, false, every, now), "过期");
            // 建得慢的项目过期间隔拉长,同样的年纪还不算过期
            let slow = refresh_interval(Duration::from_secs(2));
            assert!(!needs_rebuild(true, false, old, false, slow, now));
        }
        assert!(
            !needs_rebuild(false, true, None, true, every, now),
            "在建时不重复起"
        );
    }

    /// 过期间隔随上次建索引的耗时拉长,夹在上下限之间。
    #[test]
    fn 过期间隔按建索引耗时伸缩() {
        assert_eq!(refresh_interval(Duration::ZERO), REFRESH_AFTER);
        assert_eq!(refresh_interval(Duration::from_millis(50)), REFRESH_AFTER);
        assert_eq!(
            refresh_interval(Duration::from_secs(2)),
            Duration::from_secs(40)
        );
        assert_eq!(refresh_interval(Duration::from_secs(60)), MAX_REFRESH_AFTER);
    }

    #[test]
    fn 淘汰当前项目之外最久没用的缓存() {
        let now = Instant::now();
        let at = |secs: u64| now + Duration::from_secs(secs);
        let entries = vec![
            ("a".to_string(), at(1)),
            ("b".to_string(), at(2)),
            ("c".to_string(), at(3)),
        ];
        assert_eq!(eviction_victim(&entries, "c", 3), None, "没超上限不淘汰");
        let mut more = entries.clone();
        more.push(("d".to_string(), at(4)));
        assert_eq!(eviction_victim(&more, "d", 3), Some("a".to_string()));
        // 当前项目哪怕最久没用也不淘汰
        assert_eq!(eviction_victim(&more, "a", 3), Some("b".to_string()));
    }

    #[test]
    fn 命中下标合并成区间() {
        assert_eq!(char_runs(&[]), Vec::<(usize, usize)>::new());
        assert_eq!(char_runs(&[0, 1, 2, 5, 7, 8]), vec![(0, 3), (5, 6), (7, 9)]);
    }

    /// 命中下标按段拆开:落在文件名的换成段内下标,落在目录的原样保留,分隔符那一位两边都不算。
    #[test]
    fn 一行拆成文件名与目录两段() {
        // "src/lib.rs":`s`=0 `/`=3 `l`=4
        let (name, name_hits, dir, dir_hits) = split_row("src/lib.rs", "lib.rs", &[0, 3, 4, 5]);
        assert_eq!((name, dir), ("lib.rs", "src"));
        assert_eq!(name_hits, vec![0, 1]);
        assert_eq!(dir_hits, vec![0]);

        // 根目录下的文件没有目录段
        let (name, name_hits, dir, dir_hits) = split_row("README.md", "README.md", &[0, 1]);
        assert_eq!((name, dir), ("README.md", ""));
        assert_eq!(name_hits, vec![0, 1]);
        assert!(dir_hits.is_empty());

        // 中文按字符计
        let (_, name_hits, dir, dir_hits) = split_row("文档/说明.md", "说明.md", &[0, 3, 4]);
        assert_eq!(dir, "文档");
        assert_eq!(name_hits, vec![0, 1]);
        assert_eq!(dir_hits, vec![0]);
    }

    #[test]
    fn 底栏状态() {
        use mt_i18n::{Locale, set_locale};
        set_locale(Locale::Zh);

        let items = status_items(Some((3, 120, true)), false, None, None, false);
        assert_eq!(items.len(), 1);
        assert!(items[0].0.contains('3') && !items[0].1, "{items:?}");

        let items = status_items(Some((120, 120, false)), true, None, None, false);
        assert!(items[0].0.contains("120"), "空查询报文件总数:{items:?}");
        assert_eq!(items.len(), 2, "刷新中多一句");

        let items = status_items(
            Some((1, 10, true)),
            false,
            Some(Incomplete::TooManyFiles),
            Some(Goto {
                line: 42,
                column: None,
            }),
            false,
        );
        assert_eq!(items.len(), 3);
        assert!(items[1].1, "不完整那句用警示色");
        assert!(items[1].0.contains(&MAX_FILES.to_string()), "{items:?}");
        assert!(items[2].0.contains("42"), "{items:?}");
        for (text, _) in &items {
            assert!(!text.contains('{'), "占位符没换干净:{text}");
        }

        // 只输了 `:行` 时跳行说明在列表区,底栏不再重复
        let items = status_items(
            None,
            false,
            Some(Incomplete::TimedOut),
            Some(Goto {
                line: 7,
                column: None,
            }),
            true,
        );
        assert_eq!(items.len(), 1);
        assert!(items[0].1);
    }

    /// 方向键要先落到面板自己的动作上,压过输入框自带的 MoveUp / MoveDown(机制见
    /// `project_switcher` 的同名测试)。
    #[test]
    fn 方向键绑定压过输入框自带的() {
        gpui::actions!(quick_open_test, [InputMoveUp]);
        let predicate = format!("{CONTEXT} > Input");
        let keymap = Keymap::new(vec![
            // 组件库先注册(gpui_component::init)
            KeyBinding::new("up", InputMoveUp, Some("Input")),
            // 壳后注册(hotkeys::bind_keys)
            KeyBinding::new("up", QuickOpenPrev, Some(predicate.as_str())),
        ]);
        let stack: Vec<KeyContext> = ["Workspace", "Dialog", CONTEXT, "Input"]
            .iter()
            .map(|c| KeyContext::parse(c).unwrap())
            .collect();
        let (bindings, _) = keymap.bindings_for_input(&[Keystroke::parse("up").unwrap()], &stack);
        assert!(!bindings.is_empty());
        assert!(
            bindings[0].action().partial_eq(&QuickOpenPrev),
            "第一个派发的必须是快速打开的动作,实际是 {:?}",
            bindings[0].action()
        );
    }
}

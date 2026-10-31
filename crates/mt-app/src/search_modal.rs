//! 全局搜索(Ctrl+Shift+F)。对应 `src/components/SearchModal.tsx`,
//! 后端是 [`mt_project::search`]。
//!
//! # 线程与取消
//!
//! [`mt_project::search::run_search`] 是**阻塞**调用(遍历整棵目录树、逐行跑正则),
//! 放主线程上等于把 UI 按死。这里走 crate 自带的
//! [`start_search`](mt_project::search::start_search):它起一条专用后台线程、立刻返回
//! [`SearchHandle`],结果通过 sink 回来。sink 只做一件事 —— 往 `futures::mpsc`
//! 无界 channel 丢事件,由主线程上一个前台任务 `await` 出来改状态。
//!
//! **不用 `background_executor().spawn`**:那是给「会 await 的 future」用的执行器,
//! 塞一个跑几秒的同步闭包进去会占死它的一根工作线程(文件树列目录、外部编辑器
//! 拉起都在同一个池子里排队)。
//!
//! 取消不再走原版那条「前端发 `cancel_search` 命令 → 后端查 id」的 IPC 链路,
//! 而是谁拿着 `SearchHandle` 谁能取消。「迟到的旧结果覆盖新结果」这类竞态也不必再
//! 靠比对 `searchId` 挡:重开一次搜索时**整个换掉**那个前台任务,上一条 channel 的
//! 接收端随之被丢弃,旧 worker 往里发也送不到谁手上。
//!
//! # 与原版的偏差(逐条,理由见各处注释)
//!
//! 1. 分组头在原版是 `sticky top-0`,gpui 没有 sticky,画成普通行。
//! 2. 原版**没有**输入去抖 —— 搜索只在 Enter / 点「搜索」时发起(内容搜索是
//!    ripgrep 级别的重活,边打字边搜会把磁盘打满)。这里照此,不加去抖。

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Entity, Global, InteractiveElement,
    IntoElement, ParentElement, Pixels, Render, SharedString, Size, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, div, prelude::FluentBuilder, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use mt_project::search::{
    SearchEvent, SearchHandle, SearchMode, SearchRequest, SearchResultItem, start_search,
};
use mt_ui::tooltip::TooltipExt as _;

use crate::i18n::{t, tr};
use crate::menu;
use crate::notify::ToastKind;
use crate::overlay::kind;
use crate::prompt::{autofocus, close_guarded, open_guarded};
use crate::store::{AppStore, StoreEvent};
use crate::ui;

/// 结果上限。与原版 `SearchModal.tsx` 里那两个字面量 1000 同一个数
/// (超出后只显示前 1000 条并挂一条提示)。直接取后端的同名常量:后端收满这个数
/// 就提前收工,两边必须是同一个数。
const MAX_RESULTS: usize = mt_project::search::MAX_RESULTS;

/// Keep the row alive for the same interval GPUI uses to form `click_count=2`.
/// Windows exposes a user-configurable threshold; add a small scheduling margin
/// so the second mouse-up is delivered before the overlay closes.
#[cfg(windows)]
fn result_preview_close_delay() -> Duration {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;

    // SAFETY: `GetDoubleClickTime` reads process-independent system settings and
    // has no pointer or lifetime preconditions.
    let configured_ms = unsafe { GetDoubleClickTime() };
    let system_delay = Duration::from_millis(u64::from(configured_ms));
    system_delay.saturating_add(Duration::from_millis(50))
}

/// GPUI's Linux backends use 400 ms. The 500 ms fallback also matches the
/// existing interaction on platforms where GPUI does not expose the interval.
#[cfg(not(windows))]
fn result_preview_close_delay() -> Duration {
    Duration::from_millis(500)
}

struct GlobalSearchModal(Entity<SearchModal>);
impl Global for GlobalSearchModal {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Idle,
    Searching,
    Done,
}

pub struct SearchModal {
    store: Entity<AppStore>,
    /// Identity of the local project that produced the currently displayed
    /// results. A late result must never be reinterpreted against a newly
    /// active project (especially an SSH project with the same textual path).
    search_project: Option<(String, PathBuf)>,
    query: Entity<InputState>,
    mode: SearchMode,
    use_regex: bool,
    status: Status,
    results: Vec<SearchResultItem>,
    /// 后端报的命中数。后端收满 [`MAX_RESULTS`] 条即提前收工,此时它就是上限、
    /// `truncated` 为真(后面可能还有,状态条显示成「1000+」)。
    total_count: u32,
    truncated: bool,
    handle: Option<SearchHandle>,
    /// 结果泵。换一次搜索就整个替换 —— 旧任务被丢弃,旧 worker 的结果自然到不了。
    _pump: Option<Task<()>>,
    /// 单击结果后短暂保留浮层，让同一行的第二击仍能到达。替换或丢弃句柄即取消。
    _close_task: Option<Task<()>>,
    close_generation: u64,
    /// 结果虚拟列表的滚动位置。**必须存在视图上**:`v_virtual_list` 内部给
    /// `base` 挂的是 `track_scroll`,而 gpui 一旦 track 了句柄就以句柄里的
    /// offset 为准、不再读元素自己的跨帧状态(`elements/div.rs:2267`)——
    /// 每帧新建一个句柄等于每帧把滚动位置清零。
    results_scroll: VirtualListScrollHandle,
    _subs: Vec<Subscription>,
}

impl Drop for SearchModal {
    fn drop(&mut self) {
        // 实体在应用生命周期内常驻；这里只处理应用退出/实体真正销毁。
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
    }
}

fn overlay_open() -> bool {
    crate::overlay::contains(crate::overlay::key(kind::GLOBAL_SEARCH))
}

/// Ctrl+Shift+F:开着就关、关着就开(原版 `setSearchModalOpen(!open)`)。
pub fn toggle(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    if close_guarded(kind::GLOBAL_SEARCH, window, cx) {
        return;
    }
    open(store, window, cx);
}

/// 打开搜索框。已经开着(且上面压着别的弹窗)时是空操作 —— 防叠开在
/// [`open_guarded`] 里。
pub fn open(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    // 守卫要在**建视图之前**判一次:`open_guarded` 里那道判定拦下来的时候,
    // 下面那个输入框已经建好、`window.defer` 也已经排上了聚焦 —— 而它永远不会被
    // 画出来,焦点等于被送进虚空(终端从此收不到键)。与 `show_prompt` 同一个坑。
    if overlay_open() {
        return;
    }
    let project = {
        let store = store.read(cx);
        store.active_project().map(|project| {
            (
                project.id.clone(),
                project.name.clone(),
                store.is_remote_project(&project.id),
            )
        })
    };
    let Some((project_id, project_name, is_remote)) = project else {
        return;
    };
    if is_remote {
        crate::toast::push_message(
            ToastKind::WslInfo,
            project_id,
            project_name,
            t("search", "remoteUnsupported").to_string(),
            cx,
        );
        return;
    }
    let view = if let Some(global) = cx.try_global::<GlobalSearchModal>() {
        global.0.clone()
    } else {
        let view = cx.new(|cx| SearchModal::new(store, window, cx));
        cx.set_global(GlobalSearchModal(view.clone()));
        view
    };
    view.update(cx, |view, _cx| view.cancel_pending_close());
    let input = view.read(cx).query.clone();

    open_guarded(kind::GLOBAL_SEARCH, window, cx, {
        let view = view.clone();
        move |dialog, window, _cx| {
            let viewport = window.viewport_size();
            // 原版 `w-[80vw] h-[70vh] max-w-[900px]` + `align="center"`
            let width = (viewport.width * 0.8).min(px(900.0));
            let height = viewport.height * 0.7;
            dialog
                .p_0()
                // 头部有自己的 ✕;上游 `close_button` 是绝对定位到面板右上角的一颗,
                // `p_0()` 满幅布局下会压住自绘头部,两颗都开就是两个 ×。
                // (2026-09-19 前的理由「0.5.1 不带 svg 资产、画出来是空白」已随
                // 入口挂上 `gpui_kit_assets::Assets` 作废。)
                .close_button(false)
                // 输了半天的查询词,误点遮罩就没了 —— 原版 `closeOnOverlay={false}`
                .overlay_closable(false)
                .w(width)
                // Dialog 只认「距顶多少」,居中就是 (100% - 70%) / 2
                .margin_top(viewport.height * 0.15)
                .child(div().h(height).child(view.clone()))
        }
    });

    // Dialog 打开时会把焦点抢到自己面板上,聚焦输入框必须排在它后面
    // (判据全文见 `prompt::autofocus`)
    autofocus(&input, window, cx);
}

impl SearchModal {
    fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(placeholder_key(SearchMode::FileName))
        });
        let sub = cx.subscribe(&query, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.run(cx);
            }
        });
        // 活动项目换了 / 被删了 / 变成远程的,旧结果就作废。只在活动项目可能变了时
        // 比对(`StoreEvent::touches_active_project`)。与改造前一样只在作废时 notify
        // (render 里「搜索钮可不可点」那一处读取随根视图重画,不靠这里)
        let project_sub = cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
            if event.touches_active_project()
                && this.search_project.is_some()
                && this.current_search_root(cx).is_none()
            {
                this.reset(cx);
                cx.notify();
            }
        });
        Self {
            store,
            search_project: None,
            query,
            mode: SearchMode::FileName,
            use_regex: false,
            status: Status::Idle,
            results: Vec::new(),
            total_count: 0,
            truncated: false,
            handle: None,
            _pump: None,
            _close_task: None,
            close_generation: 0,
            results_scroll: VirtualListScrollHandle::new(),
            _subs: vec![sub, project_sub],
        }
    }

    fn project_snapshot(&self, cx: &App) -> Option<(String, PathBuf)> {
        let store = self.store.read(cx);
        let project = store.active_project()?;
        if store.is_remote_project(&project.id) {
            return None;
        }
        Some((project.id.clone(), PathBuf::from(&project.path)))
    }

    fn current_search_root(&self, cx: &App) -> Option<PathBuf> {
        let expected = self.search_project.as_ref()?;
        if self.project_snapshot(cx).as_ref() == Some(expected) {
            Some(expected.1.clone())
        } else {
            None
        }
    }

    /// 换搜索模式:取消在跑的那次、清空结果(原版那个 `useEffect([mode])`)。
    fn set_mode(&mut self, mode: SearchMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.reset(cx);
        self.query.update(cx, |state, cx| {
            state.set_placeholder(placeholder_key(mode), window, cx);
        });
        cx.notify();
    }

    /// 停掉当前搜索并清空结果。
    fn reset(&mut self, _cx: &mut Context<Self>) {
        self.cancel_pending_close();
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
        self._pump = None;
        self.results.clear();
        self.total_count = 0;
        self.truncated = false;
        self.status = Status::Idle;
        self.search_project = None;
    }

    fn cancel_pending_close(&mut self) {
        self.close_generation = self.close_generation.wrapping_add(1);
        self._close_task = None;
    }

    fn schedule_overlay_close(
        &mut self,
        expected_project: (String, PathBuf),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pending_close();
        let generation = self.close_generation;
        self._close_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(result_preview_close_delay())
                .await;
            let _ = this.update_in(cx, |this: &mut SearchModal, window, cx| {
                let active_project = this.project_snapshot(cx);
                if !preview_close_context_matches(
                    this.close_generation,
                    generation,
                    this.search_project.as_ref(),
                    active_project.as_ref(),
                    &expected_project,
                ) {
                    return;
                }
                if close_guarded(kind::GLOBAL_SEARCH, window, cx) {
                    crate::workbench_area::reactivate_active_document(
                        &expected_project.0,
                        window,
                        cx,
                    );
                }
            });
        }));
    }

    /// 发起一次搜索(Enter / 点「搜索」)。
    fn run(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().trim().to_string();
        let Some((project_id, root)) = self.project_snapshot(cx) else {
            return;
        };
        if query.is_empty() {
            return;
        }
        self.reset(cx);
        self.search_project = Some((project_id, root.clone()));

        let (tx, mut rx) = mpsc::unbounded::<SearchEvent>();
        let request = SearchRequest {
            project_root: root,
            query,
            mode: self.mode,
            use_regex: self.use_regex,
        };
        // 空 query / 非法正则在这里就被拒(不起线程,也就永远等不到 Complete),
        // 所以失败要自己把状态收成 Done,不然界面卡在「搜索中」
        let handle = match start_search(request, move |event| {
            let _ = tx.unbounded_send(event);
        }) {
            Ok(handle) => handle,
            Err(err) => {
                eprintln!("[search] 启动失败: {err:#}");
                self.status = Status::Done;
                cx.notify();
                return;
            }
        };

        self.handle = Some(handle);
        self.status = Status::Searching;
        self._pump = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                let done = this
                    .update(cx, |this: &mut SearchModal, cx| {
                        this.apply(event);
                        cx.notify();
                        this.status == Status::Done
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn apply(&mut self, event: SearchEvent) {
        match event {
            SearchEvent::Results(items) => append_capped(&mut self.results, items, MAX_RESULTS),
            SearchEvent::Complete {
                total_count,
                truncated,
                ..
            } => {
                self.status = Status::Done;
                self.total_count = total_count;
                self.truncated = truncated;
            }
        }
    }

    /// 点一条结果：单击先在工作区打开并延迟收起，双击取消延迟并交给外部编辑器。
    fn open_result(
        &mut self,
        item: &SearchResultItem,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.current_search_root(cx) else {
            return;
        };
        let Some(expected_project) = self.search_project.clone() else {
            return;
        };
        let path = root.join(&item.file_path);
        match result_action(click_count) {
            ResultAction::Preview => {
                crate::workbench_area::open_active_file(
                    self.store.clone(),
                    path,
                    // 内容搜索给命中行,文件名搜索没有行号
                    item.line_number.map(crate::file_viewer::LineTarget::line),
                    window,
                    cx,
                );
                self.schedule_overlay_close(expected_project, window, cx);
            }
            ResultAction::ExternalEditor => {
                self.cancel_pending_close();
                let editor = crate::fs_ops::configured_editor(self.store.read(cx).config());
                // 弹窗随即关掉,失败的 toast 不会被遮罩挡住
                crate::fs_ops::open_external(crate::fs_ops::ExternalOpen::Editor(editor), path, cx);
                close_guarded(kind::GLOBAL_SEARCH, window, cx);
            }
        }
    }

    /// 结果的绝对路径文本(右键「复制文件地址」用)。
    ///
    /// 分隔符按项目根里出现的是哪一种来拼(远程/WSL 路径是 POSIX 的),
    /// 与原版 `projectRoot.includes('\\') ? '\\' : '/'` 同口径 —— `Path::join`
    /// 在 Windows 上永远给 `\`,复制出来的串就与装机版不一致了。
    fn absolute_text(&self, item: &SearchResultItem, cx: &App) -> Option<String> {
        let root = self.current_search_root(cx)?.to_string_lossy().into_owned();
        let sep = if root.contains('\\') { '\\' } else { '/' };
        Some(format!("{root}{sep}{}", item.file_path.display()))
    }
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultAction {
    /// 单击：在内置文件工作区打开。
    Preview,
    /// 双击及以上：交给配置的外部编辑器。
    ExternalEditor,
}

/// 把 GPUI 的连续点击次数映射为搜索结果动作。
pub fn result_action(click_count: usize) -> ResultAction {
    if click_count >= 2 {
        ResultAction::ExternalEditor
    } else {
        ResultAction::Preview
    }
}

fn preview_close_context_matches(
    current_generation: u64,
    expected_generation: u64,
    search_project: Option<&(String, PathBuf)>,
    active_project: Option<&(String, PathBuf)>,
    expected_project: &(String, PathBuf),
) -> bool {
    current_generation == expected_generation
        && search_project == Some(expected_project)
        && active_project == Some(expected_project)
}

/// 往结果集里追加一批,总数封顶在 `cap`。
///
/// 逐条对照原版那段 `setResults`:**已经满了就整批丢弃**(不是丢最旧的),
/// 没满则只取还装得下的前几条。
pub fn append_capped<T>(results: &mut Vec<T>, batch: Vec<T>, cap: usize) {
    if results.len() >= cap {
        return;
    }
    let remaining = cap - results.len();
    results.extend(batch.into_iter().take(remaining));
}

/// 内容搜索按文件分组,**保持首次出现的顺序**(原版用 `Map`,靠的正是插入序)。
/// 返回 `(文件相对路径, 该文件的命中下标列表)`。
pub fn group_by_file(results: &[SearchResultItem]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (idx, item) in results.iter().enumerate() {
        let key = item.file_path.display().to_string();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(idx),
            None => groups.push((key, vec![idx])),
        }
    }
    groups
}

/// 结果区展平成的一维行表。虚拟列表按**下标**取行,分组头与命中行因此要摊在
/// 同一张表上(原来是 `for 分组 { 头; for 命中 { 行 } }` 的嵌套建元素)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultRow {
    /// 内容搜索的分组头:文件名 / 相对路径 / 该文件下的命中数。
    Group {
        name: SharedString,
        path: SharedString,
        count: usize,
    },
    /// 文件名搜索的一行(下标指向 `results`)。
    File(usize),
    /// 内容搜索的一条命中(下标指向 `results`)。
    Hit(usize),
    /// 末尾那条「结果已截断」提示条 —— 它本来就在滚动区里,跟着一起虚拟化。
    Truncated,
}

/// 结果集 → 行表。文件名模式一行一条;内容模式按 [`group_by_file`] 的顺序
/// 摊平成「头 + 若干命中」;满 `cap` 时末尾补一条截断提示。
pub fn result_rows(results: &[SearchResultItem], mode: SearchMode, cap: usize) -> Vec<ResultRow> {
    let mut rows = Vec::new();
    if !results.is_empty() {
        match mode {
            SearchMode::FileName => rows.extend((0..results.len()).map(ResultRow::File)),
            SearchMode::FileContent => {
                for (path, indices) in group_by_file(results) {
                    rows.push(ResultRow::Group {
                        name: results[indices[0]].file_name.clone().into(),
                        path: path.into(),
                        count: indices.len(),
                    });
                    rows.extend(indices.into_iter().map(ResultRow::Hit));
                }
            }
        }
    }
    if results.len() >= cap {
        rows.push(ResultRow::Truncated);
    }
    rows
}

/// 每种行有多高。虚拟列表要在**不建元素**的前提下拿到每行高度
/// (`v_virtual_list` 的 `item_sizes`),所以行高不能由内容撑起来 —— 每种行
/// 都钉成「上下留白 + 一行文字」,行元素本身也写同一个高度值,与
/// `git_diff` 里 uniform_list 那套是同一个口径(高度对不上就会裁字或留缝)。
///
/// 一行文字有多高按窗口当下的口径现算:`TextStyle::line_height` 是相对量
/// (默认 φ),跟着**行内最大字号**走,再按设备像素对齐 —— 与 gpui 画文本时
/// (`elements/text.rs`)算的是同一个数,因此换 UI 字号 / 换 DPI 都不用改常量。
#[derive(Clone, Copy)]
struct RowMetrics {
    group: Pixels,
    file: Pixels,
    hit: Pixels,
    truncated: Pixels,
}

impl RowMetrics {
    fn measure(window: &Window) -> Self {
        let style = window.text_style();
        let rem = window.rem_size();
        let line = |font: f32| {
            window.pixel_snap(style.line_height.to_pixels(ui::font_px(font).into(), rem))
        };
        Self {
            // 分组头 py(6)、文件名行 py(6)(行内最大字号 12)、
            // 命中行 py(4)、截断条 py(8)
            group: line(10.0) + px(12.0),
            file: line(12.0) + px(12.0),
            hit: line(10.0) + px(8.0),
            truncated: line(10.0) + px(16.0),
        }
    }

    fn of(&self, row: &ResultRow) -> Pixels {
        match row {
            ResultRow::Group { .. } => self.group,
            ResultRow::File(_) => self.file,
            ResultRow::Hit(_) => self.hit,
            ResultRow::Truncated => self.truncated,
        }
    }
}

/// 底部状态条那一句。四个分支逐条对照原版。
///
/// 与原版的偏差:后端收满上限就提前收工,不再数完整命中数,`truncated` 时总数只是
/// 下限,显示成「1000+」。
fn status_text(
    status: Status,
    mode: SearchMode,
    shown: usize,
    total: u32,
    truncated: bool,
) -> String {
    let total = if truncated {
        format!("{total}+")
    } else {
        total.to_string()
    };
    match status {
        Status::Searching => tr!("search", "searchingFound", count = shown),
        Status::Done => match mode {
            SearchMode::FileName => tr!("search", "foundFiles", count = total),
            SearchMode::FileContent => tr!("search", "foundMatches", count = total),
        },
        // 平台修饰键名。原版是 `MOD_LABEL`(mac 上 ⌘,其余 Ctrl)。
        //
        // 这条走 `t_args` 而不是 `tr!`:占位符名就叫 `mod`,而 `tr!` 的参数位是
        // `$name:ident`,`mod` 是 Rust 关键字塞不进去(写 `r#mod` 会被
        // `stringify!` 原样打成 "r#mod",对不上字典里的 `{mod}`)。
        Status::Idle => mt_i18n::t_args("search", "shortcutHint", &[("mod", mod_label())]),
    }
}

fn mod_label() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }
}

fn placeholder_key(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::FileName => t("search", "placeholderFilename"),
        SearchMode::FileContent => t("search", "placeholderContent"),
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 一段高亮文本(关键词命中处黄底黄字),对应原版 `HighlightText`。
fn highlighted(text: &str, ranges: &[(usize, usize)], size: f32) -> impl IntoElement {
    highlighted_on(text, ranges, size, ui::text_primary())
}

/// 同 [`highlighted`],但未命中片段用指定颜色——按路径搜时高亮落在弱化色的路径行上,
/// 未命中部分得保持弱化色而不是被抬成主文字色。
fn highlighted_on(
    text: &str,
    ranges: &[(usize, usize)],
    size: f32,
    base: gpui::Hsla,
) -> impl IntoElement {
    let mut row = div().flex().items_center().overflow_hidden();
    for (chunk, hit) in ui::highlight_runs(text, ranges) {
        row = row.child(
            div()
                .flex_none()
                .text_size(ui::font_px(size))
                .when(hit, |el| {
                    el.px(px(1.0))
                        .rounded(px(4.0))
                        .bg(ui::with_alpha(ui::color_warning(), 0.3))
                        .text_color(ui::color_warning())
                })
                .when(!hit, |el| el.text_color(base))
                .child(SharedString::from(chunk)),
        );
    }
    row
}

impl SearchModal {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        let seg = |id: &'static str, label: &'static str, value: SearchMode| {
            let active = mode == value;
            div()
                .id(id)
                .px(px(10.0))
                .py(px(4.0))
                .text_size(ui::font_px(10.0))
                .cursor_pointer()
                .when(active, |el| el.bg(ui::accent()).text_color(ui::bg_base()))
                .when(!active, |el| {
                    el.text_color(ui::text_muted())
                        .hover(|el| el.text_color(ui::text_primary()))
                })
                .child(label)
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .flex_none()
            .px(px(16.0))
            .py(px(12.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::accent())
                            .child(t("search", "title")),
                    )
                    .child(
                        div()
                            .flex()
                            .rounded(px(4.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(ui::border_default())
                            .child(
                                seg(
                                    "search-mode-filename",
                                    t("search", "modeFilename"),
                                    SearchMode::FileName,
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_mode(SearchMode::FileName, window, cx);
                                })),
                            )
                            .child(
                                seg(
                                    "search-mode-content",
                                    t("search", "modeContent"),
                                    SearchMode::FileContent,
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_mode(SearchMode::FileContent, window, cx);
                                })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("search-close")
                    .px(px(4.0))
                    .text_size(ui::font_px(15.0))
                    .text_color(ui::text_muted())
                    .cursor_pointer()
                    .hover(|el| el.text_color(ui::text_primary()))
                    .on_click(cx.listener(|_this, _event, window, cx| {
                        window.defer(cx, |window, cx| {
                            close_guarded(kind::GLOBAL_SEARCH, window, cx);
                        });
                    }))
                    .child("✕"),
            )
    }

    fn render_query_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_search = !self.query.read(cx).value().trim().is_empty()
            && self.status != Status::Searching
            && self.project_snapshot(cx).is_some();
        let regex_on = self.use_regex;

        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .flex_none()
            .px(px(16.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(div().flex_1().child(Input::new(&self.query).cleanable(false)))
            .child(
                div()
                    .id("search-regex")
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(4.0))
                    .border_1()
                    .text_size(ui::font_px(10.0))
                    .cursor_pointer()
                    .when(regex_on, |el| {
                        el.bg(ui::accent())
                            .text_color(ui::bg_base())
                            .border_color(ui::accent())
                    })
                    .when(!regex_on, |el| {
                        el.text_color(ui::text_muted())
                            .border_color(ui::border_default())
                            .hover(|el| el.text_color(ui::text_primary()))
                    })
                    .tip(t("search", "regexTitle"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.use_regex = !this.use_regex;
                        cx.notify();
                    }))
                    .child(".*"),
            )
            .child(
                div()
                    .id("search-run")
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(4.0))
                    .bg(ui::accent())
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::bg_base())
                    .when(!can_search, |el| el.opacity(0.5))
                    .when(can_search, |el| {
                        el.cursor_pointer().hover(|el| el.opacity(0.9))
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        if this.status != Status::Searching {
                            this.run(cx);
                        }
                    }))
                    .child(t("search", "searchButton")),
            )
    }

    /// 右键菜单:只有「复制文件地址」一项(与原版一致)。
    fn result_menu(
        &self,
        item: &SearchResultItem,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(text) = self.absolute_text(item, cx) else {
            return;
        };
        let entries = vec![menu::item(t("search", "copyFilePath"), move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        })];
        menu::show(position, entries, window, cx);
    }

    /// ⚠️ 行根必须 `w_full` + 钉死 `h`:虚拟列表把每一行当根元素单量
    /// (`layout_as_root(Definite(列宽) × Definite(行高))`),不给 `w_full`
    /// 时 flex 行按 fit-content 收窄、hover 底色只盖住文字那一截;高度与
    /// `RowMetrics` 对不上则裁字或留缝。
    fn render_result_row(&self, index: usize, h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let item = &self.results[index];
        let line_no = item.line_number.map(|n| n.to_string()).unwrap_or_default();
        let content = item.line_content.clone().unwrap_or_default();
        let ranges = item.match_ranges.clone();

        div()
            .id(SharedString::from(format!("search-row-{index}")))
            .flex()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .h(h)
            .px(px(16.0))
            .cursor_pointer()
            .hover(|el| el.bg(ui::border_subtle()))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    let Some(item) = this.results.get(index).cloned() else {
                        return;
                    };
                    this.open_result(&item, event.click_count(), window, cx);
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(item) = this.results.get(index).cloned() else {
                        return;
                    };
                    this.result_menu(&item, event.position, window, cx);
                }),
            )
            .child(
                div()
                    .w(px(40.0))
                    .flex_none()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(line_no),
            )
            .child(highlighted(&content, &ranges, 10.0))
            .into_any_element()
    }

    /// 见 [`Self::render_result_row`] 的 `w_full` / `h` 注释。
    fn render_filename_row(&self, index: usize, h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let item = &self.results[index];
        let name = item.file_name.clone();
        let path = item.file_path.display().to_string();
        // 按路径搜(查询串带 `/`)时命中区间落在路径上,高亮跟着落到路径那一行;
        // 文件名那一行照常显示但不高亮
        let (ranges, path_ranges) = if item.match_in_path {
            (Vec::new(), item.match_ranges.clone())
        } else {
            (item.match_ranges.clone(), Vec::new())
        };

        div()
            .id(SharedString::from(format!("search-file-{index}")))
            .flex()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .h(h)
            .px(px(16.0))
            .cursor_pointer()
            .hover(|el| el.bg(ui::border_subtle()))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    let Some(item) = this.results.get(index).cloned() else {
                        return;
                    };
                    this.open_result(&item, event.click_count(), window, cx);
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(item) = this.results.get(index).cloned() else {
                        return;
                    };
                    this.result_menu(&item, event.position, window, cx);
                }),
            )
            .child(highlighted(&name, &ranges, 12.0))
            .child(if path_ranges.is_empty() {
                div()
                    .min_w(px(0.0))
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .truncate()
                    .child(path)
                    .into_any_element()
            } else {
                highlighted_on(&path, &path_ranges, 10.0, ui::text_muted()).into_any_element()
            })
            .into_any_element()
    }

    /// 分组头。原版是 `sticky top-0`,gpui 没有 sticky,画成普通行(见模块注释)。
    fn render_group_row(
        &self,
        name: &SharedString,
        path: &SharedString,
        count: usize,
        h: Pixels,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .h(h)
            .px(px(16.0))
            .bg(ui::bg_elevated())
            .text_size(ui::font_px(10.0))
            .text_color(ui::accent())
            .child(div().flex_none().child(name.clone()))
            .child(
                div()
                    .min_w(px(0.0))
                    .text_color(ui::text_muted())
                    .truncate()
                    .child(path.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(ui::text_muted())
                    .child(format!("({count})")),
            )
            .into_any_element()
    }

    /// 「只显示前 N 条」提示条。
    fn render_truncated_row(&self, h: Pixels) -> AnyElement {
        div()
            .flex()
            .items_center()
            .w_full()
            .h(h)
            .px(px(16.0))
            .bg(ui::bg_elevated())
            .text_size(ui::font_px(10.0))
            .text_color(ui::color_warning())
            .child(t("search", "truncated"))
            .into_any_element()
    }

    /// 结果区。最多 [`MAX_RESULTS`] 条,**只建可见的那几行**:
    /// `v_virtual_list` 支持逐行不同高(分组头 / 命中行 / 文件名行 / 截断条
    /// 四种),行高由 [`RowMetrics`] 预先算出,定位走前缀和 + 二分。
    fn render_results(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.results.is_empty() {
            let hint = match self.status {
                Status::Searching => Some(t("search", "searching")),
                Status::Idle => Some(t("search", "idleHint")),
                Status::Done => None,
            };
            let mut body = div().size_full();
            if let Some(hint) = hint {
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h_full()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(hint),
                );
            }
            return body.into_any_element();
        }

        let metrics = RowMetrics::measure(window);
        let rows = Rc::new(result_rows(&self.results, self.mode, MAX_RESULTS));
        let sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
            rows.iter()
                .map(|row| size(px(0.0), metrics.of(row)))
                .collect(),
        );
        v_virtual_list(cx.entity(), "search-results", sizes, {
            let rows = rows.clone();
            move |this, range, _window, cx| {
                range
                    .filter_map(|ix| {
                        let row = rows.get(ix)?;
                        Some(match row {
                            ResultRow::Group { name, path, count } => {
                                this.render_group_row(name, path, *count, metrics.group)
                            }
                            ResultRow::File(index) => {
                                this.render_filename_row(*index, metrics.file, cx)
                            }
                            ResultRow::Hit(index) => {
                                this.render_result_row(*index, metrics.hit, cx)
                            }
                            ResultRow::Truncated => this.render_truncated_row(metrics.truncated),
                        })
                    })
                    .collect()
            }
        })
        .track_scroll(&self.results_scroll)
        // 组件库给 base 挂的是双轴 `overflow_scroll`,这里关掉横轴,与换掉的
        // `overflow_y_scroll()` 一致:横向内容宽是拿**第一行**量出来的,首帧还
        // 没有列宽可量时量的是 min-content(一整行不回绕的命中文本),会凭空
        // 多出一段横向可滚范围
        .overflow_x_hidden()
        .into_any_element()
    }
}

impl Render for SearchModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 虚拟列表自带 `size_full`,撑开那一格交给外面这层 `flex_1`
        // (原来是滚动容器自己 `flex_1`);`overflow_hidden` 顺带把 flex 项的
        // 自动最小高度压成 0,不然 1000 行的内容高会把这一格顶出弹窗。
        let list = div()
            .flex_1()
            .overflow_hidden()
            .bg(ui::bg_base())
            .child(self.render_results(window, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(self.render_header(cx))
            .child(self.render_query_row(cx))
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .px(px(16.0))
                    .py(px(6.0))
                    .border_t_1()
                    .border_color(ui::border_subtle())
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(status_text(
                        self.status,
                        self.mode,
                        self.results.len(),
                        self.total_count,
                        self.truncated,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, line: u32) -> SearchResultItem {
        SearchResultItem {
            file_path: PathBuf::from(path),
            file_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            line_number: Some(line),
            line_content: Some(format!("line {line}")),
            match_ranges: vec![(0, 4)],
            match_in_path: false,
        }
    }

    #[test]
    fn 单击预览双击及以上打开外部编辑器() {
        assert_eq!(result_action(0), ResultAction::Preview);
        assert_eq!(result_action(1), ResultAction::Preview);
        assert_eq!(result_action(2), ResultAction::ExternalEditor);
        assert_eq!(result_action(3), ResultAction::ExternalEditor);
    }

    #[test]
    fn 延迟关闭只在同一代搜索与同一活动项目中交还焦点() {
        let expected = ("project-a".to_string(), PathBuf::from("/project-a"));
        assert!(preview_close_context_matches(
            7,
            7,
            Some(&expected),
            Some(&expected),
            &expected,
        ));

        let other = ("project-b".to_string(), PathBuf::from("/project-b"));
        assert!(!preview_close_context_matches(
            8,
            7,
            Some(&expected),
            Some(&expected),
            &expected,
        ));
        assert!(!preview_close_context_matches(
            7,
            7,
            Some(&expected),
            Some(&other),
            &expected,
        ));
        assert!(!preview_close_context_matches(
            7,
            7,
            Some(&other),
            Some(&expected),
            &expected,
        ));
    }

    /// 满了就整批丢弃(不是丢最旧的),没满只取装得下的前几条。
    #[test]
    fn 结果封顶在一千条() {
        let mut results: Vec<u32> = Vec::new();
        append_capped(&mut results, (0..600).collect(), MAX_RESULTS);
        assert_eq!(results.len(), 600);
        // 第二批只装得下 400 条
        append_capped(&mut results, (600..1200).collect(), MAX_RESULTS);
        assert_eq!(results.len(), MAX_RESULTS);
        assert_eq!(results.last(), Some(&999), "取的是这一批的前几条");
        // 已经满了,再来一批整批丢弃
        append_capped(&mut results, (0..10).collect(), MAX_RESULTS);
        assert_eq!(results.len(), MAX_RESULTS);
    }

    /// 按文件分组要**保持首次出现的顺序**,同一文件的命中攒在一处。
    #[test]
    fn 内容结果按文件分组且保序() {
        let results = vec![
            item("src/a.rs", 1),
            item("src/b.rs", 7),
            item("src/a.rs", 9),
            item("src/b.rs", 2),
            item("src/c.rs", 3),
        ];
        let groups = group_by_file(&results);
        assert_eq!(
            groups
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("src/a.rs", vec![0, 2]),
                ("src/b.rs", vec![1, 3]),
                ("src/c.rs", vec![4]),
            ]
        );
    }

    #[test]
    fn 空结果集分组为空() {
        assert!(group_by_file(&[]).is_empty());
    }

    /// 虚拟列表按下标取行,所以分组头与命中行要摊在同一张表上:内容模式是
    /// 「头 + 该文件的命中」按分组顺序铺开,文件名模式一行一条,没有分组头。
    #[test]
    fn 结果行表按分组摊平() {
        let results = vec![
            item("src/a.rs", 1),
            item("src/b.rs", 7),
            item("src/a.rs", 9),
        ];
        assert_eq!(
            result_rows(&results, SearchMode::FileContent, MAX_RESULTS),
            vec![
                ResultRow::Group {
                    name: "a.rs".into(),
                    path: "src/a.rs".into(),
                    count: 2,
                },
                ResultRow::Hit(0),
                ResultRow::Hit(2),
                ResultRow::Group {
                    name: "b.rs".into(),
                    path: "src/b.rs".into(),
                    count: 1,
                },
                ResultRow::Hit(1),
            ]
        );
        assert_eq!(
            result_rows(&results, SearchMode::FileName, MAX_RESULTS),
            vec![ResultRow::File(0), ResultRow::File(1), ResultRow::File(2)]
        );
        // 空结果没有任何行(空态另画,不进虚拟列表)
        assert!(result_rows(&[], SearchMode::FileContent, MAX_RESULTS).is_empty());
        // 满了才在末尾补截断条,它也是一行(原来挂在滚动容器末尾)
        assert_eq!(
            result_rows(&results, SearchMode::FileName, 3).last(),
            Some(&ResultRow::Truncated)
        );
        assert!(
            !result_rows(&results, SearchMode::FileName, 4).contains(&ResultRow::Truncated),
            "没满不挂截断条"
        );
    }

    /// 状态条四个分支:搜索中报**已显示条数**、结束报后端的**完整总数**、
    /// 空闲报快捷键提示。
    #[test]
    fn 状态条四个分支() {
        use mt_i18n::{Locale, set_locale};
        set_locale(Locale::Zh);

        let searching = status_text(Status::Searching, SearchMode::FileName, 42, 0, false);
        assert!(searching.contains("42"), "{searching}");

        let files = status_text(Status::Done, SearchMode::FileName, 7, 900, false);
        assert!(files.contains("900"), "结束态报总数而不是已显示数:{files}");
        assert!(!files.contains('+'), "{files}");
        let matches = status_text(Status::Done, SearchMode::FileContent, 7, 900, false);
        assert_ne!(files, matches, "文件名 / 内容两种模式文案不同");
        // 后端收满上限提前收工:总数只是下限
        let capped = status_text(Status::Done, SearchMode::FileContent, 1000, 1000, true);
        assert!(capped.contains("1000+"), "{capped}");

        let idle = status_text(Status::Idle, SearchMode::FileName, 0, 0, false);
        assert!(idle.contains(mod_label()), "{idle}");
        assert!(!idle.contains('{'), "占位符没换干净:{idle}");
    }

    /// 两种模式的占位串不同(换模式时要跟着换)。
    #[test]
    fn 两种模式占位串不同() {
        assert_ne!(
            placeholder_key(SearchMode::FileName),
            placeholder_key(SearchMode::FileContent)
        );
    }
}

//! Git 面板的「提交历史」区。对应 `src/components/GitHistoryContent.tsx`(399 行)。
//!
//! # 分页与请求作废(`GitHistoryContent.tsx:234-290`)
//!
//! - 每页 30 条;**首页带 `branch`,续页一律 `branch: None`** ——
//!   分页从上一页末尾 commit 的 parent 续走,不需要 branch;
//! - **去重是必需的**:有分支时续页会带回已加载的 commit,重复 hash 会让拓扑图
//!   连线算错,还可能用同一个游标死循环请求。去重后
//!   `has_more = 本页 >= 30 && 合并后长度 > 之前长度`;
//! - **请求令牌**:换仓库 / 换分支时令牌 +1,迟到响应直接丢弃。
//!
//! # 拓扑图为什么自绘
//!
//! gpui 的 `svg()` 是单色 alpha 掩膜(丢色),Image 的 SVG 分支有 BGRA 交换 bug ——
//! 两条都不能用(K 批记档)。这里走 [`gpui::PathBuilder`] 自绘:贝塞尔用
//! `cubic_bezier_to`,渐变用**分段近似**(见 [`GraphCell`] 的注释)。
//!
//! ⚠️ 行高必须恒为 [`GRAPH_ROW_HEIGHT`](crate::git_graph::GRAPH_ROW_HEIGHT) = 48px,
//! 否则连线跨行接不上。

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardItem, Context, Element, Entity,
    EventEmitter, GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, ParentElement, PathBuilder, Pixels, Point, Render,
    SharedString, StatefulInteractiveElement, Style, Styled, Window, div,
    point, px, uniform_list,
};
use mt_project::git::{BranchInfo, GitCommitInfo};

use crate::git_graph::{
    self, GRAPH_ROW_HEIGHT, GraphLayout, GraphRow, SegPath, palette_color, segment_path,
};
use crate::i18n::{t, tr};
use crate::menu;
use crate::store::AppStore;
use crate::{git_diff, git_watch};
use crate::ui;

/// 每页条数(`GitHistoryContent.tsx:244`)。
const PAGE_SIZE: usize = 30;

/// 往上冒的事件。
pub enum GitHistoryEvent {
    /// pty-output 嗅探命中 → 容器要重新发现仓库(原版的 `refreshRepos()`)。
    RefreshRepos,
}

pub struct GitHistoryContent {
    store: Entity<AppStore>,
    /// 空串 = 无仓库。
    repo_path: String,
    /// 只用来给提交行标注分支胶囊。
    branches: Vec<BranchInfo>,
    /// 正在查看(未 checkout)的分支;`None` = 跟随 HEAD。
    view_branch: Option<String>,
    commits: Vec<GitCommitInfo>,
    graph: GraphLayout,
    loading: bool,
    has_more: bool,
    /// 请求令牌:换仓库 / 换分支后的迟到响应丢弃。
    request: u64,
    /// pty-output 嗅探的 500ms 去抖终点(与「更改」区各算各的)。
    debounce_until: Option<Instant>,
}

impl EventEmitter<GitHistoryEvent> for GitHistoryContent {}

impl GitHistoryContent {
    pub fn new(store: Entity<AppStore>) -> Self {
        Self {
            store,
            repo_path: String::new(),
            branches: Vec::new(),
            view_branch: None,
            commits: Vec::new(),
            graph: git_graph::compute(&[]),
            loading: false,
            has_more: false,
            request: 0,
            debounce_until: None,
        }
    }

    /// 容器把仓库 / 分支列表 / 查看分支一起透下来。任一变化都重头拉第一页
    /// (原版靠 `key={historyRefreshKey}` 与 effect 依赖达到同样效果)。
    pub fn sync(
        &mut self,
        repo_path: &str,
        branches: &[BranchInfo],
        view_branch: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let branches_changed = self.branches.len() != branches.len()
            || self
                .branches
                .iter()
                .zip(branches)
                .any(|(a, b)| a.name != b.name || a.is_head != b.is_head);
        let reload = self.repo_path != repo_path || self.view_branch.as_deref() != view_branch;
        self.repo_path = repo_path.to_string();
        self.view_branch = view_branch.map(str::to_string);
        if branches_changed {
            self.branches = branches.to_vec();
            cx.notify();
        }
        if reload {
            self.reload(cx);
        }
    }

    /// 整体重来(换仓库 / 换分支 / 提交成功 / 手动刷新)。
    ///
    /// 原版是给 `GitHistoryContent` 换 `key` 整体重建,滚动位置与已加载分页
    /// 全部丢弃 —— 这里的效果一致(规格 §11 第 26 条,原版行为,照抄)。
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.commits.clear();
        self.graph = git_graph::compute(&[]);
        self.has_more = false;
        self.request += 1;
        // ⚠️ loading 必须复位:令牌已 +1,在途响应注定被丢弃,而丢弃分支
        // 不会走到 `loading = false` —— 不复位的话这次 load_page 被 loading
        // 闸挡掉,历史区就永远停在旧仓库的内容上再也不刷
        self.loading = false;
        self.load_page(true, cx);
    }

    /// 容器的 pty-output 嗅探命中了。
    pub fn note_pty_hit(&mut self) {
        self.debounce_until = Some(Instant::now() + Duration::from_millis(git_watch::DEBOUNCE_MS));
    }

    /// 容器的节拍。
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.debounce_until.is_some_and(|at| Instant::now() >= at) {
            self.debounce_until = None;
            // 原版去抖回调里做两件事:refreshRepos() + load()
            cx.emit(GitHistoryEvent::RefreshRepos);
            self.reload(cx);
        }
    }

    fn load_page(&mut self, first: bool, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() || self.loading {
            return;
        }
        self.loading = true;
        let req = self.request;
        let repo = std::path::PathBuf::from(&self.repo_path);
        // 首页带 branch;续页从上一页末尾 commit 的 parent 续走,不需要 branch
        let branch = if first {
            self.view_branch.clone()
        } else {
            None
        };
        let before = if first {
            None
        } else {
            self.commits.last().map(|c| c.hash.clone())
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::get_git_log(
                        &repo,
                        before.as_deref(),
                        Some(PAGE_SIZE),
                        branch.as_deref(),
                    )
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.request != req {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(page) => this.merge_page(page),
                    Err(err) => eprintln!("[git] 取提交历史失败: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 合并一页。去重 + `has_more` 判定,见模块注释。
    fn merge_page(&mut self, page: Vec<GitCommitInfo>) {
        let before_len = self.commits.len();
        let full_page = page.len() >= PAGE_SIZE;
        for commit in page {
            if self.commits.iter().any(|c| c.hash == commit.hash) {
                continue;
            }
            self.commits.push(commit);
        }
        self.has_more = full_page && self.commits.len() > before_len;
        self.graph = git_graph::compute(&self.commits);
    }

    /// 触底:再要一页。
    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.has_more && !self.loading {
            self.load_page(false, cx);
        }
    }

    /// 双击行 / 右键「查看变更」:先取文件列表,再开 CommitDiffModal。
    fn view_commit(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(index) else {
            return;
        };
        let repo = self.repo_path.clone();
        let hash = commit.hash.clone();
        let message = commit.message.clone();
        let store = self.store.clone();
        let repo_for_task = std::path::PathBuf::from(&repo);
        let hash_for_task = hash.clone();
        cx.spawn_in(window, async move |_this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::get_commit_files(&repo_for_task, &hash_for_task)
                })
                .await;
            let files = match files {
                Ok(files) => files,
                Err(err) => {
                    // 原版 `console.error` 静默
                    eprintln!("[git] 取 commit 文件列表失败: {err:#}");
                    return;
                }
            };
            let _ = cx.update(|window, cx| {
                git_diff::open_commit_diff(store, repo, hash, message, files, window, cx);
            });
        })
        .detach();
    }

    /// 该行要标注的分支胶囊。
    ///
    /// ⚠️ **只标注本工作区的分支**(`GitHistoryContent.tsx:166-169` 原注释):
    /// worktree 与主仓库共享 refs,标出全部分支会把其他工作区/远程的分支全挂到
    /// commit 上,看起来像本工作区持有它们。
    fn shown_branches(&self, hash: &str) -> Vec<&BranchInfo> {
        self.branches
            .iter()
            .filter(|b| b.is_head || Some(b.name.as_str()) == self.view_branch.as_deref())
            .filter(|b| b.commit_hash == hash)
            .collect()
    }
}

/// 相对时间(`src/utils/timeFormat.ts:3-18`)。
///
/// ⚠️ 命名空间是 **`time`**,不是 `session_panel` 用的 `sessionList.time.*` ——
/// 两套 key 并存,别串。30 天以上是**纯数字日期**,没有 `time.monthDay` 这种 key。
pub fn format_relative_time(timestamp: i64, now: i64) -> String {
    let diff = now - timestamp;
    if diff < 60 {
        return t("time", "justNow").to_string();
    }
    if diff < 3600 {
        return tr!("time", "minutesAgo", n = (diff / 60).to_string());
    }
    if diff < 86400 {
        return tr!("time", "hoursAgo", n = (diff / 3600).to_string());
    }
    if diff < 2_592_000 {
        return tr!("time", "daysAgo", n = (diff / 86400).to_string());
    }
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        // 时间戳坏到本地时区都换算不出来时退回原值,别 panic
        None => timestamp.to_string(),
    }
}

impl Render for GitHistoryContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("git-history-body")
            .flex_1()
            .min_h(px(0.0))
            .overflow_hidden()
            .px(px(4.0))
            .py(px(4.0));

        if self.repo_path.is_empty() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(ui::bg_surface())
                .child(body.child(hint(t("gitHistoryContent", "noRepos"), 13.0)));
        }

        if self.commits.is_empty() {
            return div().size_full().flex().flex_col().bg(ui::bg_surface()).child(
                body.child(hint(
                    if self.loading {
                        t("gitHistoryContent", "loading")
                    } else {
                        t("gitHistoryContent", "noCommits")
                    },
                    11.0,
                )),
            );
        }

        let count = self.commits.len();
        let this = cx.entity();
        let list = uniform_list(
            "git-commit-list",
            count,
            move |range, _window, cx: &mut App| {
                // 触底再要一页。**延到下一拍**执行:这里还在窗口的 prepaint 里,
                // 直接 update 自己会与正在进行的渲染打架。
                if range.end >= count {
                    let this = this.clone();
                    cx.spawn(async move |cx| {
                        let _ = this.update(cx, |this: &mut GitHistoryContent, cx| {
                            this.load_more(cx)
                        });
                    })
                    .detach();
                }
                let this = this.clone();
                range
                    .map(|i| this.update(cx, |this, cx| this.render_commit(i, cx)))
                    .collect::<Vec<_>>()
            },
        )
        .size_full();

        body = body.child(list);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(body)
    }
}

fn hint(text: &'static str, size: f32) -> AnyElement {
    div()
        .py(px(if size > 12.0 { 24.0 } else { 8.0 }))
        .w_full()
        .text_center()
        .text_size(ui::font_px(size))
        .text_color(ui::text_muted())
        .child(text)
        .into_any_element()
}

impl GitHistoryContent {
    fn render_commit(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(commit) = self.commits.get(index) else {
            return div().into_any_element();
        };
        let Some(row) = self.graph.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let now = chrono::Utc::now().timestamp();
        let hash_full = commit.hash.clone();
        let short_hash = commit.short_hash.clone();
        let author = commit.author.clone();
        let message = commit.message.clone();
        let relative = format_relative_time(commit.timestamp, now);

        let mut first_line = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .min_w(px(0.0))
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_primary());
        for branch in self.shown_branches(&commit.hash) {
            let (bg, fg) = if branch.is_head {
                (palette_color(0), gpui::white())
            } else if branch.is_remote {
                (ui::border_subtle(), ui::text_muted())
            } else {
                (
                    ui::with_alpha(mt_ui::rgb8(63, 185, 80), 0.2),
                    mt_ui::rgb8(63, 185, 80),
                )
            };
            first_line = first_line.child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .rounded(px(3.0))
                    .bg(bg)
                    .text_color(fg)
                    .child(branch.name.clone()),
            );
        }
        first_line = first_line.child(div().truncate().child(message));

        div()
            .id(SharedString::from(format!("git-commit-{hash_full}")))
            .flex()
            .items_center()
            .h(px(GRAPH_ROW_HEIGHT))
            .px(px(8.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|el| el.bg(ui::border_subtle()))
            .child(GraphCell {
                row,
                width: self.graph.width,
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .pl(px(4.0))
                    .child(first_line)
                    .child(
                        div()
                            .mt(px(2.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(div().max_w(px(140.0)).truncate().child(author))
                            .child("·")
                            .child(div().flex_none().child(relative))
                            .child("·")
                            .child(div().flex_none().child(short_hash)),
                    ),
            )
            // ⚠️ 是**双击**不是单击(`GitHistoryContent.tsx:105`)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.view_commit(index, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |_this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entity = cx.entity();
                    let hash = hash_full.clone();
                    let entries = vec![
                        menu::item(
                            t("gitHistoryContent", "copyCommitHash"),
                            move |_window, cx| {
                                // 完整 hash,不是 shortHash
                                cx.write_to_clipboard(ClipboardItem::new_string(hash.clone()));
                            },
                        ),
                        menu::separator(),
                        menu::item(t("gitHistoryContent", "viewChanges"), move |window, cx| {
                            entity.update(cx, |this, cx| this.view_commit(index, window, cx));
                        }),
                    ];
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .into_any_element()
    }
}

// ─── 拓扑图单元格(自绘) ─────────────────────────────────────

/// 一行的拓扑图。宽度 = [`GraphLayout::width`],高度恒为 48px。
///
/// # 渐变的取舍
///
/// 原版每条异色线段挂一个 `<linearGradient>`(三个 stop:0% color / 70% color /
/// 100% endColor)。gpui 的 `paint_path` 只吃单色,这里改成**分段近似**:
/// 把路径按参数 t 切成若干小段,`t < 0.7` 用起点色,之后线性插值到终点色。
/// 渐变是纯装饰(分支线并入主线时的过渡),分段够密时肉眼无差。
struct GraphCell {
    row: GraphRow,
    width: f32,
}

/// 渐变分段数。段与段之间留一点重叠(见 `paint`),8 段在 48px 行高上已看不出台阶。
const GRADIENT_STEPS: usize = 8;

impl IntoElement for GraphCell {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

fn lerp_color(a: Hsla, b: Hsla, t: f32) -> Hsla {
    // 在 RGB 上插值:HSL 上插值会绕色相环(蓝→绿会经过青,原版 SVG 是 RGB 插值)
    let (a, b) = (gpui::Rgba::from(a), gpui::Rgba::from(b));
    gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
    .into()
}

/// 贝塞尔 / 直线上按参数 t 取点。
fn sample(path: &SegPath, t: f32) -> (f32, f32) {
    match path {
        SegPath::Line { x, y0, y1 } => (*x, y0 + (y1 - y0) * t),
        SegPath::Cubic { p0, c1, c2, p1 } => {
            let u = 1.0 - t;
            let (b0, b1, b2, b3) = (
                u * u * u,
                3.0 * u * u * t,
                3.0 * u * t * t,
                t * t * t,
            );
            (
                b0 * p0.0 + b1 * c1.0 + b2 * c2.0 + b3 * p1.0,
                b0 * p0.1 + b1 * c1.1 + b2 * c2.1 + b3 * p1.1,
            )
        }
    }
}

impl Element for GraphCell {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = px(self.width).into();
        style.size.height = px(GRAPH_ROW_HEIGHT).into();
        style.flex_shrink = 0.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        let origin = bounds.origin;
        let map = |(x, y): (f32, f32)| -> Point<Pixels> {
            point(origin.x + px(x), origin.y + px(y))
        };

        for seg in &self.row.segments {
            let Some(path) = segment_path(seg, self.row.lane) else {
                continue;
            };
            let color = palette_color(seg.color);
            if !git_graph::needs_gradient(seg) {
                let mut builder = PathBuilder::stroke(px(1.5));
                match path {
                    SegPath::Line { x, y0, y1 } => {
                        builder.move_to(map((x, y0)));
                        builder.line_to(map((x, y1)));
                    }
                    SegPath::Cubic { p0, c1, c2, p1 } => {
                        builder.move_to(map(p0));
                        builder.cubic_bezier_to(map(p1), map(c1), map(c2));
                    }
                }
                if let Ok(built) = builder.build() {
                    window.paint_path(built, color);
                }
                continue;
            }

            // 异色:按 t 分段,0..0.7 用起点色,之后插值到终点色
            let end = palette_color(seg.end_color.unwrap_or(seg.color));
            for step in 0..GRADIENT_STEPS {
                let t0 = step as f32 / GRADIENT_STEPS as f32;
                // 段尾多伸一点点,免得相邻段之间露出缝
                let t1 = ((step + 1) as f32 / GRADIENT_STEPS as f32 + 0.02).min(1.0);
                let mid = (t0 + t1) * 0.5;
                let color = if mid <= 0.7 {
                    color
                } else {
                    lerp_color(color, end, (mid - 0.7) / 0.3)
                };
                let mut builder = PathBuilder::stroke(px(1.5));
                builder.move_to(map(sample(&path, t0)));
                // 分段本身用折线近似:一段只占整条曲线的 1/8,直线误差 < 0.5px
                builder.line_to(map(sample(&path, mid)));
                builder.line_to(map(sample(&path, t1)));
                if let Ok(built) = builder.build() {
                    window.paint_path(built, color);
                }
            }
        }

        // 节点圆(`GitHistoryContent.tsx:68-75`)
        let cx_pos = git_graph::lane_x(self.row.lane as i32);
        let cy = GRAPH_ROW_HEIGHT / 2.0;
        let color = palette_color(self.row.color);
        if self.row.is_merge {
            // 合并:空心大圈(半透明)+ 实心小点
            paint_circle(window, map, (cx_pos, cy), 5.5, None, {
                let mut c = color;
                c.a *= 0.55;
                c
            });
            paint_circle(window, map, (cx_pos, cy), 3.0, Some(()), color);
        } else {
            paint_circle(window, map, (cx_pos, cy), 4.0, Some(()), color);
        }
    }
}

/// 画一个圆。`fill` 为 `Some` 时实心,否则 1.5px 描边。
fn paint_circle(
    window: &mut Window,
    map: impl Fn((f32, f32)) -> Point<Pixels>,
    center: (f32, f32),
    r: f32,
    fill: Option<()>,
    color: Hsla,
) {
    const SEGMENTS: usize = 20;
    let mut builder = match fill {
        Some(()) => PathBuilder::fill(),
        None => PathBuilder::stroke(px(1.5)),
    };
    for i in 0..SEGMENTS {
        let theta = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        let p = map((center.0 + r * theta.cos(), center.1 + r * theta.sin()));
        if i == 0 {
            builder.move_to(p);
        } else {
            builder.line_to(p);
        }
    }
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_graph::test_commit;

    /// 去重:有分支时续页会带回已加载的 commit,重复 hash 必须丢掉,
    /// 否则拓扑图连线会算错。
    #[test]
    fn 分页去重与_has_more() {
        // 借一个不需要 store 的壳来测纯逻辑:merge_page 只碰 commits/graph/has_more
        struct Fake {
            commits: Vec<GitCommitInfo>,
            has_more: bool,
        }
        impl Fake {
            fn merge(&mut self, page: Vec<GitCommitInfo>) {
                let before_len = self.commits.len();
                let full_page = page.len() >= PAGE_SIZE;
                for commit in page {
                    if self.commits.iter().any(|c| c.hash == commit.hash) {
                        continue;
                    }
                    self.commits.push(commit);
                }
                self.has_more = full_page && self.commits.len() > before_len;
            }
        }

        let mut fake = Fake {
            commits: Vec::new(),
            has_more: false,
        };
        // 满页 30 条 → 还有更多
        let page: Vec<_> = (0..PAGE_SIZE)
            .map(|i| test_commit(&format!("c{i}"), &[]))
            .collect();
        fake.merge(page.clone());
        assert_eq!(fake.commits.len(), PAGE_SIZE);
        assert!(fake.has_more);

        // 整页都是重复的 → 合并后长度没涨 → 停止分页(否则会用同一个游标死循环)
        fake.merge(page.clone());
        assert_eq!(fake.commits.len(), PAGE_SIZE, "重复 hash 必须丢掉");
        assert!(!fake.has_more);

        // 不满页 → 到底了
        fake.merge(vec![test_commit("tail", &[])]);
        assert_eq!(fake.commits.len(), PAGE_SIZE + 1);
        assert!(!fake.has_more);
    }

    /// 相对时间四个档位的边界(59/60/3599/3600/86399/86400/2591999/2592000)。
    #[test]
    fn 相对时间四档边界() {
        let now = 1_700_000_000i64;
        // 「刚刚」这一档没有占位符,直接比字面量
        assert_eq!(format_relative_time(now, now), t("time", "justNow"));
        assert_eq!(format_relative_time(now - 59, now), t("time", "justNow"));

        // 分 / 时 / 天三档各自与 `tr!` 的展开一致
        assert_eq!(
            format_relative_time(now - 60, now),
            tr!("time", "minutesAgo", n = "1".to_string())
        );
        assert_eq!(
            format_relative_time(now - 3599, now),
            tr!("time", "minutesAgo", n = "59".to_string())
        );
        assert_eq!(
            format_relative_time(now - 3600, now),
            tr!("time", "hoursAgo", n = "1".to_string())
        );
        assert_eq!(
            format_relative_time(now - 86_399, now),
            tr!("time", "hoursAgo", n = "23".to_string())
        );
        assert_eq!(
            format_relative_time(now - 86_400, now),
            tr!("time", "daysAgo", n = "1".to_string())
        );
        assert_eq!(
            format_relative_time(now - 2_591_999, now),
            tr!("time", "daysAgo", n = "29".to_string())
        );

        // 满 30 天换成纯数字日期(YYYY-MM-DD),没有任何本地化词条
        let date = format_relative_time(now - 2_592_000, now);
        assert_eq!(date.len(), 10, "{date}");
        assert_eq!(date.matches('-').count(), 2, "{date}");
    }

    /// 贝塞尔采样:两端点必须落在路径端点上(渐变分段接得住)。
    #[test]
    fn 贝塞尔采样端点对齐() {
        let cubic = SegPath::Cubic {
            p0: (7.0, 0.0),
            c1: (7.0, 12.0),
            c2: (21.0, 12.0),
            p1: (21.0, 24.0),
        };
        assert_eq!(sample(&cubic, 0.0), (7.0, 0.0));
        let end = sample(&cubic, 1.0);
        assert!((end.0 - 21.0).abs() < 0.001 && (end.1 - 24.0).abs() < 0.001);
        // 中点落在两端之间
        let mid = sample(&cubic, 0.5);
        assert!(mid.0 > 7.0 && mid.0 < 21.0);

        let line = SegPath::Line {
            x: 7.0,
            y0: 24.0,
            y1: 48.0,
        };
        assert_eq!(sample(&line, 0.0), (7.0, 24.0));
        assert_eq!(sample(&line, 1.0), (7.0, 48.0));
        assert_eq!(sample(&line, 0.5), (7.0, 36.0));
    }
}

//! 源码编辑器的「注解」(Annotate / `git blame`):[`FileViewer`] 的又一半 `impl`。
//!
//! 工具栏「注解」开关打开后,编辑器左侧**额外占一列**(不盖行号):每行左缘一条按提交
//! 新旧着色的细色条(accent 五档透明度,越新越亮),同一提交连续的行只在首行写
//! 「作者 日期」(滚到一半的那段在首个可见行也写),未提交的行写「未提交」。悬停浮出
//! 提交标题 / 作者 <邮箱> / 绝对时间 / 短哈希,点击打开该提交的 diff。
//!
//! 列的内容与 gutter 标记由同一张标记层在 paint 阶段画(竖向位置同一套来源,见
//! [`super::git_gutter`] 模块注释);这里只出数据、宽度与几块浮层。
//!
//! # 行号对齐
//!
//! blame 跑在**缓冲区**上(`git blame --contents -`,喂的是还原成落盘形态的当前内容),
//! 结果行号就是当时缓冲区的行号。之后再编辑不重跑 git:标记那条去抖重算顺带用
//! [`mt_project::git::line_alignment`] 把「blame 时的内容」对到当前内容上,对得上的行沿用
//! 原提交,对不上的(新敲的 / 改过的)一律算未提交。保存不影响对齐;HEAD 动了
//! (提交 / 切分支,由 gutter 的指纹轮询发现)才重跑。

use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Datelike, TimeZone};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, DispatchPhase, Font, Hitbox,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    Pixels, Point, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, Task,
    TextAlign, TextRun, WeakEntity, Window, anchored, deferred, div, fill, point,
    prelude::FluentBuilder as _, px, size,
};
use mt_project::git::{BLAME_MAX_LINES, BlameResult, RepoFile};
use mt_ui::icons::VectorIcon;
use mt_ui::tooltip::TooltipExt as _;

use super::git_gutter::{EditorGeometry, ICON_CLOSE};
use super::{DocumentSource, FileViewer};
use crate::i18n::{t, tr};
use crate::ui;

/// 注解文字字号(编辑器正文 13px,注解小一号)。
const FONT_SIZE: f32 = 12.0;
/// 色条:离列左缘 4px、宽 3px。
const STRIP_LEFT: f32 = 4.0;
const STRIP_WIDTH: f32 = 3.0;
/// 文字起点(色条右侧再留 5px)与右侧留白。
const TEXT_LEFT: f32 = 12.0;
const RIGHT_PAD: f32 = 10.0;
/// 作者名最多占几列(CJK 一字算两列),超出截断加省略号。
const AUTHOR_MAX_COLS: usize = 12;
/// 悬停多久才浮出提交详情(从一行划到另一行不闪)。
const HOVER_DELAY: Duration = Duration::from_millis(350);
/// 新旧五档的透明度,下标 0 最旧。
const TIER_ALPHA: [f32; 5] = [0.16, 0.32, 0.5, 0.7, 0.95];
/// 往回找「同一提交连续段」的起点时最多看多少行(只影响悬停判定,不影响显示)。
const RUN_SCAN_LIMIT: usize = 5_000;

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 一个字符占几列:东亚宽字符与 emoji 两列,其余一列。只用来给注解列定宽,
/// 估宽偏大无妨(宁可多留白,不许截字)。
pub(super) fn char_cols(c: char) -> usize {
    let cp = c as u32;
    let wide = matches!(cp,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
}

pub(super) fn text_cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

/// 截到不超过 `max` 列,截了就以 `…` 收尾(省略号本身算一列)。
pub(super) fn truncate_cols(s: &str, max: usize) -> String {
    if text_cols(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut cols = 0;
    for c in s.chars() {
        let w = char_cols(c);
        if cols + w + 1 > max {
            break;
        }
        out.push(c);
        cols += w;
    }
    out.push('…');
    out
}

/// 注解列里的短日期:今年的写 `10-21`,往年的写 `2024-10-21`。
pub(super) fn short_date<Tz: TimeZone>(time: i64, now: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let (Some(at), Some(now)) = (
        DateTime::from_timestamp(time, 0),
        DateTime::from_timestamp(now, 0),
    ) else {
        return String::new();
    };
    let (at, now) = (at.with_timezone(tz), now.with_timezone(tz));
    if at.year() == now.year() {
        at.format("%m-%d").to_string()
    } else {
        at.format("%Y-%m-%d").to_string()
    }
}

/// 悬停详情里的绝对时间(本机时区)。
pub(super) fn full_time<Tz: TimeZone>(time: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    DateTime::from_timestamp(time, 0)
        .map(|at| at.with_timezone(tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 按新旧排名分五档(0 最旧、4 最新,同一时刻同一档)。按排名而不是按时间线性
/// 分:一个十年前的初始提交不该把其余提交全挤进最亮的那一档。
pub(super) fn age_tiers(times: &[i64]) -> Vec<usize> {
    let mut distinct: Vec<i64> = times.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let k = distinct.len();
    times
        .iter()
        .map(|time| {
            if k <= 1 {
                return 4;
            }
            let rank = distinct.binary_search(time).unwrap_or(0);
            ((rank * 4) as f32 / (k - 1) as f32).round() as usize
        })
        .collect()
}

/// 一行的注解归属。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineBlame {
    /// `commits` 里的下标
    Commit(u32),
    /// 还没提交(blame 标的全零哈希,或 blame 之后又改过的行)
    Uncommitted,
}

/// 当前缓冲区第 `line` 行的归属。`map` 是「当前行 → blame 时的行」(`None` = 缓冲区
/// 自 blame 以来没变,行号直通);映射不到的行 = blame 之后改过 = 未提交。超出范围
/// (末尾那个空行)返回 `None`,不画。
pub(super) fn line_blame(
    result: &BlameResult,
    map: Option<&[Option<u32>]>,
    line: usize,
) -> Option<LineBlame> {
    let base_line = match map {
        Some(map) => match *map.get(line)? {
            Some(base) => base as usize,
            None => return Some(LineBlame::Uncommitted),
        },
        None => line,
    };
    let idx = *result.lines.get(base_line)?;
    let commit = result.commits.get(idx as usize)?;
    Some(if commit.is_uncommitted() {
        LineBlame::Uncommitted
    } else {
        LineBlame::Commit(idx)
    })
}

/// `line` 所在「同一归属连续段」的首行。
pub(super) fn run_start(result: &BlameResult, map: Option<&[Option<u32>]>, line: usize) -> usize {
    let blame = line_blame(result, map, line);
    let mut start = line;
    while start > 0 && line - start < RUN_SCAN_LIMIT && line_blame(result, map, start - 1) == blame
    {
        start -= 1;
    }
    start
}

// ─── 状态 ─────────────────────────────────────────────────────

/// 一个提交在注解列里的显示件(截断好的作者、短日期、色条透明度)。
pub(super) struct CommitLabel {
    pub author: SharedString,
    pub date: SharedString,
    pub alpha: f32,
}

/// 一次 blame 的结果连同显示件。
pub(super) struct BlameView {
    pub repo: RepoFile,
    pub result: BlameResult,
    /// blame 时的缓冲区(编辑器投影):之后编辑的行映射以它为基准
    pub base: SharedString,
    pub labels: Vec<CommitLabel>,
    /// 作者那一栏最宽几列、日期那一栏几列(今年 5 / 往年 10)
    pub author_cols: usize,
    pub date_cols: usize,
    /// 每次 blame 结果换代 +1,行映射靠它丢掉旧代的结果
    pub generation: u64,
}

impl BlameView {
    fn new(repo: RepoFile, result: BlameResult, base: SharedString, generation: u64) -> Self {
        let now = chrono::Utc::now().timestamp();
        let tz = chrono::Local;
        // 档位只在已提交的提交之间排:未提交的「时间」是现在,算进去会把真正最新的
        // 那次提交挤下一档
        let tiers = age_tiers(
            &result
                .commits
                .iter()
                .filter(|c| !c.is_uncommitted())
                .map(|c| c.author_time)
                .collect::<Vec<_>>(),
        );
        let mut tiers = tiers.into_iter();
        let labels: Vec<CommitLabel> = result
            .commits
            .iter()
            .map(|commit| CommitLabel {
                author: truncate_cols(&commit.author, AUTHOR_MAX_COLS).into(),
                date: short_date(commit.author_time, now, &tz).into(),
                // 未提交的行色条用 muted,不看这个值
                alpha: if commit.is_uncommitted() {
                    0.0
                } else {
                    TIER_ALPHA[tiers.next().unwrap_or(4).min(TIER_ALPHA.len() - 1)]
                },
            })
            .collect();
        let committed = || {
            result
                .commits
                .iter()
                .zip(&labels)
                .filter(|(commit, _)| !commit.is_uncommitted())
        };
        let author_cols = committed()
            .map(|(_, label)| text_cols(&label.author))
            .max()
            .unwrap_or(0);
        let date_cols = committed()
            .map(|(_, label)| label.date.len())
            .max()
            .unwrap_or(5);
        Self {
            repo,
            result,
            base,
            labels,
            author_cols,
            date_cols,
            generation,
        }
    }
}

/// 注解没能显示 / 点注解没能打开提交的原因(提示条)。
pub(super) enum BlameNotice {
    TooLarge,
    /// 完整的提示文字(已带「注解载入失败 / 打开提交失败」前缀)
    Failed(String),
}

/// 报错只取第一行:git 的错误常带好几行 hint,提示条是单行的。
fn first_line(err: &anyhow::Error) -> String {
    format!("{err:#}")
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 悬停中的那一段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct BlameHover {
    pub blame: LineBlame,
    /// 段首行(同一段里划动不换浮层)
    pub run_start: usize,
    /// 详情浮层的左上角(窗口坐标)
    pub anchor: Point<Pixels>,
    /// 悬停够 [`HOVER_DELAY`] 了,浮层可以出来
    pub shown: bool,
}

/// 一次去抖重算里注解那一份的输入:blame 时的内容 + 它的代次。
pub(super) struct BlameDiffInput {
    base: SharedString,
    generation: u64,
}

/// 行映射的结果。`map` 为 `None` = 缓冲区与 blame 时一致,行号直通。
pub(super) struct LineMap {
    generation: u64,
    map: Option<Vec<Option<u32>>>,
}

impl BlameDiffInput {
    pub(super) fn compute(&self, buffer: &str) -> LineMap {
        let map = (buffer != self.base.as_ref()).then(|| {
            mt_project::git::line_alignment(&self.base, buffer)
                .into_iter()
                .map(|line| line.map(|l| l as u32))
                .collect()
        });
        LineMap {
            generation: self.generation,
            map,
        }
    }
}

/// 一个页签的注解状态。
#[derive(Default)]
pub(super) struct Annotations {
    /// 工具栏开关(编辑器重建、HEAD 换代都保留)
    pub(super) enabled: bool,
    loading: bool,
    request: u64,
    generation: u64,
    view: Option<Rc<BlameView>>,
    map: Option<Rc<Vec<Option<u32>>>>,
    task: Option<Task<()>>,
    pub(super) notice: Option<BlameNotice>,
    hover: Option<BlameHover>,
    hover_task: Option<Task<()>>,
    /// 正在取某个提交的文件列表(防连点)
    opening: bool,
}

impl Annotations {
    /// 编辑器没了:结果与在飞的请求作废,开关留着。
    pub(super) fn drop_data(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.loading = false;
        self.view = None;
        self.map = None;
        self.task = None;
        self.clear_hover();
    }

    pub(super) fn clear_hover(&mut self) {
        self.hover = None;
        self.hover_task = None;
    }

    /// 标记那条去抖重算里要不要顺带算行映射。
    pub(super) fn diff_input(&self) -> Option<BlameDiffInput> {
        let view = self.view.as_ref().filter(|_| self.enabled)?;
        Some(BlameDiffInput {
            base: view.base.clone(),
            generation: view.generation,
        })
    }

    pub(super) fn apply_line_map(&mut self, map: LineMap) {
        if self
            .view
            .as_ref()
            .is_some_and(|view| view.generation == map.generation)
        {
            self.map = map.map.map(Rc::new);
        }
    }
}

// ─── 视图接线 ─────────────────────────────────────────────────

impl FileViewer {
    /// 工具栏要不要出「注解」开关:本地、源码态、文件在 HEAD 里。
    fn annotate_available(&self) -> bool {
        matches!(self.source, DocumentSource::Local { .. })
            && self.editor.is_some()
            && !(self.has_preview_toggle() && self.preview)
            && self.gutter.head_base().is_some()
    }

    fn toggle_annotations(&mut self, cx: &mut Context<Self>) {
        let annotate = &mut self.annotate;
        annotate.notice = None;
        if annotate.enabled {
            annotate.enabled = false;
            annotate.drop_data();
            cx.notify();
            return;
        }
        annotate.enabled = true;
        self.git_load_blame(cx);
        cx.notify();
    }

    /// HEAD 取回来了(首次 / 换代)。开着注解的话:文件不在 HEAD 里就收起,HEAD 动过或
    /// 还没有结果就重跑。
    pub(super) fn git_blame_after_head(&mut self, head_moved: bool, cx: &mut Context<Self>) {
        if !self.annotate.enabled {
            return;
        }
        if self.gutter.head_base().is_none() {
            self.annotate.enabled = false;
            self.annotate.drop_data();
            return;
        }
        if head_moved || (self.annotate.view.is_none() && !self.annotate.loading) {
            self.git_load_blame(cx);
        }
    }

    /// 保存成功:注解开着、且 blame 之后内容动过(在用行映射)就重跑一次。
    pub(super) fn git_blame_after_save(&mut self, cx: &mut Context<Self>) {
        let annotate = &self.annotate;
        if annotate.enabled && !annotate.loading && annotate.map.is_some() {
            self.git_load_blame(cx);
        }
    }

    fn git_load_blame(&mut self, cx: &mut Context<Self>) {
        // HEAD 还在取:取回来时由 `git_blame_after_head` 接着发起
        let Some(head) = self.gutter.head_base() else {
            return;
        };
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let text = editor.read(cx).value();
        if text.lines().count() > BLAME_MAX_LINES {
            self.annotate.enabled = false;
            self.annotate.drop_data();
            self.annotate.notice = Some(BlameNotice::TooLarge);
            cx.notify();
            return;
        }
        // 喂给 git 的是落盘形态(Tab、行尾按原文件),否则每一行都会被当成改过
        let on_disk = self.doc.to_disk_form(&text);
        let repo = head.repo.clone();
        let annotate = &mut self.annotate;
        annotate.request = annotate.request.wrapping_add(1);
        let req = annotate.request;
        annotate.loading = true;
        annotate.task = Some(cx.spawn(async move |this, cx| {
            let (root, rel) = (repo.root.clone(), repo.rel_path.clone());
            let outcome = cx
                .background_executor()
                .spawn(async move { mt_project::git::blame_file(&root, &rel, &on_disk) })
                .await;
            let _ = this.update(cx, |view: &mut FileViewer, cx| {
                if view.annotate.request != req {
                    return;
                }
                view.git_apply_blame(repo, text, outcome, cx);
            });
        }));
        cx.notify();
    }

    fn git_apply_blame(
        &mut self,
        repo: RepoFile,
        base: SharedString,
        outcome: anyhow::Result<BlameResult>,
        cx: &mut Context<Self>,
    ) {
        let annotate = &mut self.annotate;
        annotate.loading = false;
        annotate.task = None;
        match outcome {
            Ok(result) => {
                annotate.generation = annotate.generation.wrapping_add(1);
                annotate.view = Some(Rc::new(BlameView::new(
                    repo,
                    result,
                    base.clone(),
                    annotate.generation,
                )));
                annotate.map = None;
                annotate.clear_hover();
                // blame 跑着的时候又编辑过:立刻补一次行映射
                let current = self.editor.as_ref().map(|e| e.read(cx).value());
                if current.is_some_and(|text| text != base) {
                    self.git_recompute_now(cx);
                }
            }
            Err(err) => {
                annotate.enabled = false;
                annotate.view = None;
                annotate.map = None;
                annotate.notice = Some(BlameNotice::Failed(format!(
                    "{}: {}",
                    t("gitGutter", "annotateFailed"),
                    first_line(&err)
                )));
            }
        }
        cx.notify();
    }

    fn set_blame_hover(
        &mut self,
        next: Option<(LineBlame, usize, Point<Pixels>)>,
        cx: &mut Context<Self>,
    ) {
        let same = match (&self.annotate.hover, &next) {
            (Some(cur), Some((blame, run, _))) => cur.blame == *blame && cur.run_start == *run,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        let was_shown = self.annotate.hover.is_some_and(|h| h.shown);
        let Some((blame, run_start, anchor)) = next else {
            self.annotate.clear_hover();
            cx.notify();
            return;
        };
        // 已经浮着详情时换段直接换内容,不再等一次延迟
        self.annotate.hover = Some(BlameHover {
            blame,
            run_start,
            anchor,
            shown: was_shown,
        });
        self.annotate.hover_task = (!was_shown).then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOVER_DELAY).await;
                let _ = this.update(cx, |view: &mut FileViewer, cx| {
                    if let Some(hover) = view.annotate.hover.as_mut() {
                        hover.shown = true;
                        cx.notify();
                    }
                });
            })
        });
        cx.notify();
    }

    /// 点注解:取该提交的文件列表,打开提交 diff(与历史区同一条路,取文件失败弹详情框)。
    /// 取的这段时间里 `opening` 挡住连点。
    fn open_blame_commit(&mut self, commit: u32, window: &mut Window, cx: &mut Context<Self>) {
        if self.annotate.opening {
            return;
        }
        let Some(view) = self.annotate.view.clone() else {
            return;
        };
        let Some(info) = view.result.commits.get(commit as usize).cloned() else {
            return;
        };
        if info.is_uncommitted() {
            return;
        }
        self.annotate.opening = true;
        self.annotate.clear_hover();
        cx.notify();
        let store = crate::store::AppStore::global(cx);
        let root = view.repo.root.to_string_lossy().into_owned();
        let opening = crate::git_diff::open_commit_diff_loading(
            store,
            root,
            info.hash,
            info.summary,
            None,
            window,
            cx,
        );
        cx.spawn(async move |this, cx| {
            opening.await;
            let _ = this.update(cx, |view: &mut FileViewer, cx| {
                view.annotate.opening = false;
                cx.notify();
            });
        })
        .detach();
    }

    // ── 渲染 ─────────────────────────────────────────────

    /// 工具栏的「注解」开关。开着时描边与字染 accent(与「目录」同一套),载入中带转圈。
    pub(super) fn render_annotate_toggle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.annotate_available() {
            return None;
        }
        let on = self.annotate.enabled;
        let tip = if on {
            t("gitGutter", "hideAnnotations")
        } else {
            t("gitGutter", "showAnnotations")
        };
        Some(
            super::toolbar_outline_button("file-viewer-annotate", t("gitGutter", "annotate"))
                .flex()
                .items_center()
                .gap(px(6.0))
                .when(on, |el| {
                    el.text_color(ui::accent()).border_color(ui::accent())
                })
                .when(self.annotate.loading, |el| {
                    el.child(ui::spinner(px(12.0), ui::text_muted()))
                })
                .tip(tip)
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.toggle_annotations(cx);
                }))
                .into_any_element(),
        )
    }

    /// 注解没能显示的提示条(过大 / 失败),右侧一个关闭钮。
    pub(super) fn render_annotate_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let notice = self.annotate.notice.as_ref()?;
        let (text, color) = match notice {
            BlameNotice::TooLarge => (
                tr!("gitGutter", "annotateTooLarge", count = BLAME_MAX_LINES),
                ui::color_warning(),
            ),
            BlameNotice::Failed(text) => (text.clone(), ui::color_error()),
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .pl(px(16.0))
                .pr(px(8.0))
                .py(px(4.0))
                .border_b_1()
                .border_color(ui::border_subtle())
                .bg(ui::with_alpha(color, 0.15))
                .text_size(ui::font_px(12.0))
                .text_color(color)
                .child(div().flex_1().min_w(px(0.0)).truncate().child(text))
                .child(
                    div()
                        .id("file-viewer-annotate-notice-close")
                        .flex_none()
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|el| el.bg(ui::bg_elevated()))
                        .tip(t("gitGutter", "dismiss"))
                        .child(VectorIcon::new(ICON_CLOSE, px(14.0)).ink(color))
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.annotate.notice = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// 悬停详情浮层(不挡鼠标:没有命中框,鼠标仍落在注解列上)。
    pub(super) fn render_blame_tooltip(&self) -> Option<AnyElement> {
        if !self.annotate.enabled
            || self.editor.is_none()
            || (self.has_preview_toggle() && self.preview)
        {
            return None;
        }
        let hover = self.annotate.hover.filter(|h| h.shown)?;
        let view = self.annotate.view.as_ref()?;
        let card = div()
            .max_w(px(460.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_surface())
            .shadow_lg()
            .text_size(ui::font_px(12.0));
        let card = match hover.blame {
            LineBlame::Uncommitted => card.child(
                div()
                    .text_color(ui::text_secondary())
                    .child(t("gitGutter", "uncommittedTip")),
            ),
            LineBlame::Commit(idx) => {
                let commit = view.result.commits.get(idx as usize)?;
                let time = full_time(commit.author_time, &chrono::Local);
                card.child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(SharedString::from(commit.summary.clone())),
                )
                .child(
                    div()
                        .text_color(ui::text_secondary())
                        .child(format!("{} <{}>", commit.author, commit.author_mail)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(12.0))
                        .text_color(ui::text_muted())
                        .child(format!("{time} ({})", commit.author_tz))
                        .child(
                            div()
                                .font_family(super::editor_font_family())
                                .child(commit.short_hash().to_string()),
                        ),
                )
                .child(
                    div()
                        .pt(px(2.0))
                        .text_size(ui::font_px(11.5))
                        .text_color(ui::text_muted())
                        .child(t("gitGutter", "openCommitHint")),
                )
            }
        };
        Some(
            deferred(
                anchored()
                    .position(hover.anchor)
                    .snap_to_window_with_margin(px(8.0))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// 注解列这一帧的绘制件(render 时拍);没开 / 还没有结果时 `None`(列不占宽)。
    pub(super) fn annotation_paint(&self, window: &Window) -> Option<AnnotationPaint> {
        if !self.annotate.enabled {
            return None;
        }
        let view = self.annotate.view.clone()?;
        let font = annotation_font();
        let uncommitted: SharedString = t("gitGutter", "uncommitted").into();
        let cols = view.author_cols.max(text_cols(&uncommitted)) + 1 + view.date_cols;
        let width = px(TEXT_LEFT + RIGHT_PAD) + mono_char_width(window, &font) * cols as f32;
        Some(AnnotationPaint {
            width,
            view,
            map: self.annotate.map.clone(),
            font,
            uncommitted,
            hover: self.annotate.hover,
        })
    }
}

/// 注解列用编辑器同一套等宽字族,字号小一号。
fn annotation_font() -> Font {
    Font {
        fallbacks: Some(super::editor_font_fallbacks()),
        ..gpui::font(super::editor_font_family())
    }
}

/// 等宽字族里一个半角字符有多宽(量十个取平均)。
fn mono_char_width(window: &Window, font: &Font) -> Pixels {
    let sample: SharedString = "0000000000".into();
    let run = TextRun {
        len: sample.len(),
        font: font.clone(),
        color: ui::text_primary(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(sample, px(FONT_SIZE), &[run], None);
    shaped.width / 10.0
}

// ─── 注解列绘制 ───────────────────────────────────────────────

/// 注解列这一帧的绘制件。
pub(super) struct AnnotationPaint {
    pub width: Pixels,
    view: Rc<BlameView>,
    map: Option<Rc<Vec<Option<u32>>>>,
    font: Font,
    uncommitted: SharedString,
    /// 实体上记着的悬停段(监听据此判断「换段了没有」)
    hover: Option<BlameHover>,
}

impl AnnotationPaint {
    fn blame_of(&self, line: usize) -> Option<LineBlame> {
        line_blame(
            &self.view.result,
            self.map.as_deref().map(Vec::as_slice),
            line,
        )
    }

    /// 画注解列并挂上悬停 / 点击监听。`bounds` 是整张标记层(列在它的最左边)。
    pub(super) fn paint(
        &self,
        bounds: Bounds<Pixels>,
        geom: &EditorGeometry,
        hitbox: &Hitbox,
        view: WeakEntity<FileViewer>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let column = Bounds::new(bounds.origin, size(self.width, bounds.size.height));
        // 与编辑器之间一条分隔线
        window.paint_quad(fill(
            Bounds::new(
                point(column.origin.x + self.width - px(1.0), column.origin.y),
                size(px(1.0), column.size.height),
            ),
            ui::border_subtle(),
        ));

        let rows: Vec<(usize, Pixels, Pixels, LineBlame)> = geom
            .rows
            .iter()
            .filter_map(|row| Some((row.line, row.top, row.bottom, self.blame_of(row.line)?)))
            .collect();
        let row_at = {
            let rows = rows.clone();
            move |pos: Point<Pixels>| {
                rows.iter()
                    .copied()
                    .find(|(_, top, bottom, _)| pos.y >= *top && pos.y < *bottom)
            }
        };
        let mouse = window.mouse_position();
        let over = hitbox.is_hovered(window) && column.contains(&mouse);
        let hovered = over.then(|| row_at(mouse)).flatten().map(|row| row.3);

        let accent = ui::accent();
        let text_color = ui::text_secondary();
        let muted = ui::text_muted();
        let text_x = column.origin.x + px(TEXT_LEFT);
        let right = column.origin.x + self.width - px(RIGHT_PAD);
        for (i, &(line, top, bottom, blame)) in rows.iter().enumerate() {
            let height = bottom - top;
            if hovered == Some(blame) && blame != LineBlame::Uncommitted {
                window.paint_quad(fill(
                    Bounds::new(
                        point(column.origin.x, top),
                        size(self.width - px(1.0), height),
                    ),
                    ui::with_alpha(accent, 0.10),
                ));
            }
            let strip = match blame {
                LineBlame::Commit(idx) => {
                    ui::with_alpha(accent, self.view.labels[idx as usize].alpha)
                }
                LineBlame::Uncommitted => ui::with_alpha(muted, 0.35),
            };
            window.paint_quad(fill(
                Bounds::new(
                    point(column.origin.x + px(STRIP_LEFT), top),
                    size(px(STRIP_WIDTH), height),
                ),
                strip,
            ));
            // 段首(或首个可见行)才写字
            let first_of_run = i == 0 || line == 0 || self.blame_of(line - 1) != Some(blame);
            if !first_of_run {
                continue;
            }
            match blame {
                LineBlame::Commit(idx) => {
                    let label = &self.view.labels[idx as usize];
                    self.paint_text(&label.author, text_x, top, text_color, geom, window, cx);
                    let date = self.shape(&label.date, muted, window);
                    let _ = date.paint(
                        point(right - date.width, top),
                        geom.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
                LineBlame::Uncommitted => {
                    self.paint_text(&self.uncommitted, text_x, top, muted, geom, window, cx);
                }
            }
        }
        if hovered.is_some_and(|b| b != LineBlame::Uncommitted) {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }

        // ── 监听 ──
        let result_view = self.view.clone();
        let map = self.map.clone();
        let current = self.hover;
        let anchor_x = column.origin.x + self.width + px(8.0);
        {
            let (hitbox, view, row_at) = (hitbox.clone(), view.clone(), row_at.clone());
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let next = (hitbox.is_hovered(window) && column.contains(&event.position))
                    .then(|| row_at(event.position))
                    .flatten()
                    .map(|(line, top, _, blame)| {
                        let map = map.as_deref().map(Vec::as_slice);
                        let run = run_start(&result_view.result, map, line);
                        (blame, run, point(anchor_x, top))
                    });
                let same = match (current, next) {
                    (Some(cur), Some((blame, run, _))) => {
                        cur.blame == blame && cur.run_start == run
                    }
                    (None, None) => true,
                    _ => false,
                };
                if !same {
                    // 换了段:记下新段(延迟浮出详情),顺带重画高亮
                    let _ = view.update(cx, |view, cx| view.set_blame_hover(next, cx));
                } else if hovered != next.map(|n| n.0) {
                    // 段没换但这一帧画出来的高亮 / 光标已对不上:只重画
                    let _ = view.update(cx, |_, cx| cx.notify());
                }
            });
        }
        {
            let (hitbox, view) = (hitbox.clone(), view.clone());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                    || !column.contains(&event.position)
                {
                    return;
                }
                cx.stop_propagation();
                if let Some((_, _, _, LineBlame::Commit(idx))) = row_at(event.position) {
                    let _ = view.update(cx, |view, cx| view.open_blame_commit(idx, window, cx));
                }
            });
        }
        if current.is_some() {
            window.on_mouse_event(move |_: &ScrollWheelEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble {
                    let _ = view.update(cx, |view, cx| {
                        view.annotate.clear_hover();
                        cx.notify();
                    });
                }
            });
        }
    }

    fn shape(
        &self,
        text: &SharedString,
        color: gpui::Hsla,
        window: &mut Window,
    ) -> gpui::ShapedLine {
        let run = TextRun {
            len: text.len(),
            font: self.font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(text.clone(), px(FONT_SIZE), &[run], None)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_text(
        &self,
        text: &SharedString,
        x: Pixels,
        top: Pixels,
        color: gpui::Hsla,
        geom: &EditorGeometry,
        window: &mut Window,
        cx: &mut App,
    ) {
        let shaped = self.shape(text, color, window);
        let _ = shaped.paint(
            point(x, top),
            geom.line_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

#[cfg(test)]
#[path = "git_blame_tests.rs"]
mod tests;

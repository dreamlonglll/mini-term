//! 源码编辑器的 Git 变更标记(gutter 竖条)与点开的变更小窗:[`FileViewer`] 的
//! 又一半 `impl`。对标 IDEA 的 line status tracker:行号与正文之间一条竖条,新增绿 /
//! 修改橙 / 删除处一个红色小三角,点一下在该块下方浮出 HEAD 里的原内容与
//! 「上一处 / 下一处 / 回滚 / 完整 diff / 复制」工具条。注解列(blame)在
//! [`super::git_blame`]。
//!
//! # 标记画在哪、靠什么对齐
//!
//! gpui-component 的编辑器 gutter **没有**公开的标记 / 装饰 API(`input/base/element.rs`
//! 只画背景、行号、当前行底与折叠图标),而且不许 fork。于是在 [`Editor`] 外包一层,
//! 上面盖一张整幅的 `canvas`,**在 paint 阶段**按编辑器上一次排版的结果自己画:
//!
//! - 竖向位置全部来自公开的 [`EditorState::range_to_bounds`]:给它「一行从行首到行尾」
//!   的字节范围,拿回这一行在窗口里的矩形 —— 顶是首个视觉行的顶、底是最后一个续行的
//!   底,**软换行天然对齐**,不必自己数续行;
//! - 只问 [`EditorState::visible_row_range`] 里那几十行,每帧的量与可见行数成正比;
//! - 正文左缘(= 行号栏右缘)由同一个矩形的 x 减去 [`EditorState::text_bounds`] 的 x
//!   反推,字号缩放、行号位数变宽、DPI 125% / 150% 都跟着走(全程逻辑像素)。
//!
//! 为什么必须在 paint 里取:编辑器把这一帧的排版写回 state 是在**它自己的 paint**
//! 里(`state.last_layout = …`),而本层是它的后一个兄弟、paint 在它之后 —— 读到的
//! 就是这一帧的结果;放在 render / prepaint 里读会落后一帧,滚动时标记拖尾。
//!
//! 竖条落在行号栏右侧那 10px 留白里(组件常量 `LINE_NUMBER_RIGHT_MARGIN`,折叠图标的
//! 命中区止于正文左缘前 8px),离正文 3px,不盖行号也不盖折叠箭头。
//!
//! # 降级
//!
//! - **折叠**:被折起来的行 `range_to_bounds` 会落到折叠区之后第一个可见行上(它在
//!   可见行里找第一个能容纳该偏移的行,饱和减法让后面的行全都「容纳」)。这类行与
//!   下一行同顶,[`dedupe_hidden_rows`] 据此剔掉 —— 折叠区里的变更不画,折叠头那一行
//!   照常画;
//! - **不画**:远程文档、不在仓库、HEAD 里没有这个文件(未跟踪 / 新加入暂存区:整份都是
//!   新的,满屏绿条只是噪音,与 IDEA 一致)、HEAD 版本是二进制或超过 1 MB、预览态
//!   (Markdown / HTML 没有行的概念)。
//!
//! # 数据流
//!
//! 编辑器 Change → 去抖 [`DIFF_DEBOUNCE`] → 后台 `line_hunks(HEAD 投影, 缓冲区)`(注解开着
//! 时顺带算 blame 的行映射)→ 用请求令牌丢掉迟到结果。HEAD 版本按页签缓存:载入 /
//! 外部改动重载时重取,页签激活、保存成功、每 [`HEAD_POLL`] 轮询一次 HEAD 指纹
//! (git2 读 ref + tree,几毫秒),变了才重取。

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, ClipboardItem, ContentMask, Context, CursorStyle,
    DispatchPhase, Entity, Hitbox, HitboxBehavior, Hsla, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, PathBuilder, Pixels,
    Point, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity,
    Window, actions, anchored, canvas, deferred, div, fill, point, prelude::FluentBuilder as _, px,
    size,
};
use gpui_component::input::{Editor, EditorState, Position, RopeExt as _};
use mt_project::git::{DiffSide, HeadFile, LineChangeKind, LineHunk, RepoFile};
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};
use mt_ui::tooltip::TooltipExt as _;

use super::git_blame::{AnnotationPaint, BlameDiffInput};
use super::{DocumentSource, FileViewer, normalize_to_lf};
use crate::i18n::{t, tr};
use crate::tab_expansion::{TAB_WIDTH, TabExpansion};
use crate::ui;

actions!(
    mini_term,
    [
        /// 跳到下一处 Git 变更(Ctrl+Alt+Shift+↓,IDEA 键位)。绑定在 `FileViewer` 上。
        GitNextChange,
        /// 跳到上一处 Git 变更(Ctrl+Alt+Shift+↑)。
        GitPrevChange,
    ]
);

/// 文件查看器里「上一处 / 下一处变更」的两条绑定,由 [`crate::hotkeys::bind_keys`] 的
/// 表外段注册。谓词挂 `FileViewer`:焦点在编辑器里时沿上下文链命中;编辑器自带的
/// `ctrl-alt-up/down`(加光标)与 Workspace 的 `ctrl-shift-up/down`(AI 标记)修饰键都
/// 不一样,gpui 按修饰键精确匹配,互不相干(有单测钉住)。
pub fn git_change_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-alt-shift-down", GitNextChange, Some("FileViewer")),
        KeyBinding::new("ctrl-alt-shift-up", GitPrevChange, Some("FileViewer")),
    ]
}

/// 键位的显示串(浮层按钮的悬停说明用)。mac 上同样是 Control 而不是 Command。
fn change_key_label(down: bool) -> &'static str {
    match (cfg!(target_os = "macos"), down) {
        (true, true) => "⌃⌥⇧↓",
        (true, false) => "⌃⌥⇧↑",
        (false, true) => "Ctrl+Alt+Shift+↓",
        (false, false) => "Ctrl+Alt+Shift+↑",
    }
}

/// 编辑后多久重算一次 diff。
pub(super) const DIFF_DEBOUNCE: Duration = Duration::from_millis(300);
/// HEAD 指纹的轮询间隔(只在本页签是当前文档时真去查)。
const HEAD_POLL: Duration = Duration::from_secs(3);

/// 竖条宽度。
const BAR_WIDTH: f32 = 3.0;
/// 竖条右缘到正文左缘的距离。
const BAR_GAP: f32 = 3.0;
/// 标记条的命中 / 悬停底色宽度:行号栏右侧那段留白(折叠图标命中区止于正文前 8px)。
const STRIP_WIDTH: f32 = 8.0;
/// 删除标记(小三角)的半高与宽。
const TRIANGLE_HALF: f32 = 4.0;
const TRIANGLE_WIDTH: f32 = 5.0;
/// 删除标记的竖向命中半径。
const DELETED_HIT: f32 = 5.0;
/// 浮层宽度的上下限。
const POPUP_MIN_WIDTH: f32 = 360.0;
const POPUP_MAX_WIDTH: f32 = 760.0;
/// 浮层里旧内容最多显示多少行高(再多滚动)。
const POPUP_MAX_LINES: f32 = 14.0;
/// 浮层旧内容的行号列:每位数字的宽度(13px 等宽字一位约 7.8px,留点余量 ——
/// 编辑器字族是用户可配的,宽一点的等宽字也装得下)与左右留白。
const POPUP_DIGIT_W: f32 = 8.5;
const POPUP_NUMBER_PAD_L: f32 = 10.0;
const POPUP_NUMBER_PAD_R: f32 = 8.0;

/// 行号列的总宽(含左右留白):按最大行号的位数算。
///
/// 早先是 `7.5 × (位数 + 2)` 再减掉 8px 右留白,一位数只剩 7px 放 13px 的数字,
/// 右对齐的数字向左溢出,被浮层的圆角边框切掉半个(E2E 截到「9」只剩右半)。
fn popup_number_width(max_line: usize) -> Pixels {
    let digits = max_line.max(1).ilog10() + 1;
    px(POPUP_DIGIT_W * digits as f32 + POPUP_NUMBER_PAD_L + POPUP_NUMBER_PAD_R)
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 一行(含它的软换行续行)在窗口里的竖向范围。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RowSpan {
    /// 0-based 行号
    pub line: usize,
    pub top: Pixels,
    pub bottom: Pixels,
}

/// 剔掉折叠起来的行:它们被 `range_to_bounds` 映射到了后面第一个可见行上,与下一项
/// 同顶(见模块注释「降级」)。输入按行号升序;同顶的一串只留最后那个真身。
pub(super) fn dedupe_hidden_rows(raw: Vec<RowSpan>) -> Vec<RowSpan> {
    let mut rows: Vec<RowSpan> = Vec::with_capacity(raw.len());
    for row in raw {
        if let Some(last) = rows.last_mut()
            && last.top == row.top
        {
            *last = row;
            continue;
        }
        rows.push(row);
    }
    rows
}

/// 这一帧编辑器的排版快照(窗口坐标)。
#[derive(Clone, Debug)]
pub(super) struct EditorGeometry {
    /// 编辑器输入区:行号从它的左缘画起
    pub input: Bounds<Pixels>,
    /// 正文左缘 = 行号栏右缘
    pub text_left: Pixels,
    pub line_height: Pixels,
    /// 可见行,按行号升序,不含折叠藏起来的行
    pub rows: Vec<RowSpan>,
}

/// 从编辑器上一次排版里读出几何(只用公开 API,见模块注释)。还没排过版返回 `None`。
pub(super) fn editor_geometry(state: &EditorState) -> Option<EditorGeometry> {
    let range = state.visible_row_range()?;
    let text_bounds = state.text_bounds()?;
    let line_height = state.line_height()?;
    let input = state.input_bounds();
    let rope = state.text();
    let mut gutter_width = None;
    let mut raw = Vec::with_capacity(range.len());
    for line in range {
        let start = rope.line_start_offset(line);
        let end = rope.line_end_offset(line);
        let Some(bounds) = state.range_to_bounds(&(start..end)) else {
            continue;
        };
        // 行首字符的 x = 文字区原点 + 行号栏宽(左对齐、首个视觉行没有悬挂缩进);
        // 文字区原点带着横向滚动,相减正好消掉
        gutter_width.get_or_insert(bounds.origin.x - text_bounds.origin.x);
        raw.push(RowSpan {
            line,
            top: bounds.origin.y,
            bottom: bounds.origin.y + bounds.size.height,
        });
    }
    Some(EditorGeometry {
        input,
        text_left: input.origin.x + gutter_width?,
        line_height,
        rows: dedupe_hidden_rows(raw),
    })
}

/// gutter 上画出来的一段标记(窗口坐标)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MarkerSegment {
    /// 在变更块列表里的下标
    pub hunk: usize,
    pub kind: LineChangeKind,
    pub top: Pixels,
    /// 删除标记是一条分界线:`top == bottom`
    pub bottom: Pixels,
}

/// 变更块 × 可见行 → 要画的标记段。新增 / 修改块按行出段、相邻的行并成一段;删除块
/// 落在删除点那一行的顶(删在文件末尾时落在最后一行的底)。只遍历与可见行相交的块。
pub(super) fn marker_segments(hunks: &[LineHunk], rows: &[RowSpan]) -> Vec<MarkerSegment> {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return Vec::new();
    };
    let row_of = |line: usize| {
        rows.binary_search_by_key(&line, |r| r.line)
            .ok()
            .map(|i| rows[i])
    };
    // 块按新侧行号升序且互不重叠,`起点 + max(行数, 1)` 也单调
    let begin = hunks.partition_point(|h| h.new_start + h.new_len.max(1) <= first.line);
    let mut out = Vec::new();
    for (idx, hunk) in hunks.iter().enumerate().skip(begin) {
        if hunk.new_start > last.line + 1 {
            break;
        }
        if hunk.kind == LineChangeKind::Deleted {
            let y = row_of(hunk.new_start).map(|r| r.top).or_else(|| {
                hunk.new_start
                    .checked_sub(1)
                    .and_then(row_of)
                    .map(|r| r.bottom)
            });
            if let Some(y) = y {
                out.push(MarkerSegment {
                    hunk: idx,
                    kind: hunk.kind,
                    top: y,
                    bottom: y,
                });
            }
            continue;
        }
        let from = rows.partition_point(|r| r.line < hunk.new_start);
        let mut current: Option<MarkerSegment> = None;
        for row in rows[from..]
            .iter()
            .take_while(|r| r.line < hunk.new_start + hunk.new_len)
        {
            match current.as_mut() {
                // 软换行 / 相邻行首尾相接:并成一段
                Some(seg) if row.top <= seg.bottom + px(0.5) => seg.bottom = row.bottom,
                _ => {
                    out.extend(current.take());
                    current = Some(MarkerSegment {
                        hunk: idx,
                        kind: hunk.kind,
                        top: row.top,
                        bottom: row.bottom,
                    });
                }
            }
        }
        out.extend(current);
    }
    out
}

/// 竖坐标 `y` 落在哪一段标记上(删除标记按 [`DELETED_HIT`] 的半径算)。
pub(super) fn segment_at(segments: &[MarkerSegment], y: Pixels) -> Option<MarkerSegment> {
    segments.iter().copied().find(|seg| {
        if seg.kind == LineChangeKind::Deleted {
            (y - seg.top).abs() <= px(DELETED_HIT)
        } else {
            y >= seg.top && y < seg.bottom
        }
    })
}

/// 上一处 / 下一处变更(首尾回绕)。`current` 是浮层正开着的那一块;没有浮层时从
/// 光标所在行 `cursor_line` 起找:下一处 = 起点在光标行之后的第一块,上一处 = 整块都在
/// 光标行之前的最后一块(光标正落在某块里时,两个方向都跳过它自己)。
pub(super) fn adjacent_hunk(
    hunks: &[LineHunk],
    cursor_line: usize,
    current: Option<usize>,
    forward: bool,
) -> Option<usize> {
    let n = hunks.len();
    if n == 0 {
        return None;
    }
    if let Some(cur) = current.filter(|&c| c < n) {
        return Some(if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        });
    }
    if forward {
        Some(
            hunks
                .iter()
                .position(|h| h.new_start > cursor_line)
                .unwrap_or(0),
        )
    } else {
        Some(
            hunks
                .iter()
                .rposition(|h| h.new_start + h.new_len.max(1) <= cursor_line)
                .unwrap_or(n - 1),
        )
    }
}

/// 回滚一块:返回「缓冲区里要替换的字节范围 + 换成什么」。`text` 是缓冲区全文
/// (`\n` 行尾),`old_lines` 是这一块在 HEAD 里的原文(编辑器投影,已展开 Tab)。
pub(super) fn rollback_edit(
    text: &str,
    hunk: &LineHunk,
    old_lines: &[&str],
) -> (std::ops::Range<usize>, String) {
    // `split('\n')` 的每一段 = 一行的 [起, 止)(不含换行);以换行结尾的文本最后多一段
    // 空串,行号仍按 `lines()` 的口径数
    let mut spans = Vec::new();
    let mut at = 0;
    for part in text.split('\n') {
        spans.push((at, at + part.len()));
        at += part.len() + 1;
    }
    let line_count = text.lines().count();
    let start_of = |line: usize| spans.get(line).map_or(text.len(), |s| s.0);
    let old = old_lines.join("\n");
    // 换回去的内容落在文件最末、后面没有换行时:原文最后一行若是空行,得补一个换行
    // 它才算一行(`lines()` 口径下 "a\n" 是一行、"a\n\n" 才是 [a, ""])
    let at_eof = |block: String| {
        if old_lines.last() == Some(&"") {
            format!("{block}\n")
        } else {
            block
        }
    };
    match hunk.kind {
        LineChangeKind::Added => {
            let mut start = start_of(hunk.new_start);
            let end_line = hunk.new_start + hunk.new_len;
            let end = if end_line < line_count {
                start_of(end_line)
            } else {
                text.len()
            };
            // 删到了没有结尾换行的末行:连前一行的换行一起删,保持「末尾没有换行」。
            // 前一行本身是空行时不能删 —— 那个换行就是它作为一行存在的全部
            if end == text.len()
                && !text.ends_with('\n')
                && start > 1
                && !text[..start - 1].ends_with('\n')
            {
                start -= 1;
            }
            (start..end, String::new())
        }
        LineChangeKind::Modified => {
            let start = start_of(hunk.new_start);
            let last = hunk.new_start + hunk.new_len - 1;
            let end = spans.get(last).map_or(text.len(), |s| s.1);
            let replacement = if end == text.len() { at_eof(old) } else { old };
            (start..end, replacement)
        }
        LineChangeKind::Deleted => {
            if hunk.new_start < line_count {
                let pos = start_of(hunk.new_start);
                (pos..pos, format!("{old}\n"))
            } else if text.is_empty() {
                (0..0, at_eof(old))
            } else if text.ends_with('\n') {
                (text.len()..text.len(), format!("{old}\n"))
            } else {
                (text.len()..text.len(), at_eof(format!("\n{old}")))
            }
        }
    }
}

/// 变更块的类型 → 标记色(与 Git 状态色一致:A 绿 / M 橙 / D 红)。
pub(super) fn kind_color(kind: LineChangeKind) -> Hsla {
    match kind {
        LineChangeKind::Added => ui::color_success(),
        LineChangeKind::Modified => ui::color_warning(),
        LineChangeKind::Deleted => ui::color_error(),
    }
}

// ─── 状态 ─────────────────────────────────────────────────────

/// HEAD 版本按页签缓存的那一份。
pub(super) struct HeadBase {
    pub repo: RepoFile,
    /// 编辑器投影(归一行尾 + 展开 Tab):diff 与回滚都比这一份。`SharedString` 是
    /// 引用计数的,丢后台不必整份拷贝
    pub text: SharedString,
    /// 编辑器投影按行
    pub lines: Vec<String>,
    /// 原文按行(只归一行尾,Tab 原样):「复制原内容」给的是 HEAD 里的真实内容
    pub raw_lines: Vec<String>,
}

/// HEAD 取数的状态。
#[derive(Default)]
pub(super) enum HeadState {
    /// 没取过(远程文档 / 还没有编辑器)
    #[default]
    Idle,
    Loading,
    /// 取过了,不画标记:不在仓库 / 不在 HEAD / 二进制 / 过大 / 取数失败
    Unavailable,
    Ready(Rc<HeadBase>),
}

/// 后台把 [`HeadFile`] 归一好再交回主线程。
pub(super) struct HeadOutcome {
    repo: Option<RepoFile>,
    base: Option<HeadBase>,
}

/// 归一 HEAD 原文:与 `DocumentSession` 落基线同一顺序 —— 先折行尾、再展开 Tab。
pub(super) fn prepare_head(file: HeadFile) -> HeadOutcome {
    let repo = file.repo().cloned();
    let HeadFile::Text {
        repo: repo_file,
        text,
    } = file
    else {
        return HeadOutcome { repo, base: None };
    };
    let lf = normalize_to_lf(&text);
    let (expanded, _) = TabExpansion::expand(&lf, TAB_WIDTH);
    HeadOutcome {
        repo,
        base: Some(HeadBase {
            repo: repo_file,
            lines: expanded.lines().map(str::to_string).collect(),
            raw_lines: lf.lines().map(str::to_string).collect(),
            text: expanded.into(),
        }),
    }
}

/// 点开的变更小窗。
pub(super) struct ChangePopup {
    /// 在 [`GitGutter::hunks`] 里的下标
    pub hunk: usize,
    /// 浮层左上角(窗口坐标)。`None` = 刚导航过去、还没排到版:由标记层在 paint 里
    /// 算出来回填,回填之前不画(免得在旧位置闪一帧)
    pub anchor: Option<Point<Pixels>>,
    pub width: Pixels,
    /// 「复制原内容」点过了:按钮换成对勾
    pub copied: bool,
}

/// 一个页签的 gutter 状态。
#[derive(Default)]
pub(super) struct GitGutter {
    pub(super) head: HeadState,
    /// 文件所在仓库(HEAD 不可用时也记着,轮询指纹要用);不在仓库里为 `None`
    repo: Option<RepoFile>,
    head_request: u64,
    pub(super) hunks: Rc<Vec<LineHunk>>,
    diff_request: u64,
    diff_task: Option<Task<()>>,
    /// 下一次重算不去抖(回滚这类用户动作要当场看到结果)
    diff_now: bool,
    poll_task: Option<Task<()>>,
    poll_inflight: bool,
    pub(super) popup: Option<ChangePopup>,
}

impl GitGutter {
    /// HEAD 侧已就绪的那一份。
    pub(super) fn head_base(&self) -> Option<&Rc<HeadBase>> {
        match &self.head {
            HeadState::Ready(base) => Some(base),
            _ => None,
        }
    }
}

impl HeadBase {
    /// 一块变更在 HEAD 里的原文(编辑器投影)。块与 HEAD 不同代(HEAD 刚换、块还没
    /// 重算)时越界,返回 `None` 而不是 panic。
    pub(super) fn old_lines(&self, hunk: &LineHunk) -> Option<&[String]> {
        self.lines.get(hunk.old_range())
    }
}

// ─── 视图接线 ─────────────────────────────────────────────────

impl FileViewer {
    /// 编辑器(重)建好了:本地文档去取 HEAD 版本。
    pub(super) fn git_on_editor_installed(&mut self, cx: &mut Context<Self>) {
        self.git_on_editor_dropped();
        if self.editor.is_some() && matches!(self.source, DocumentSource::Local { .. }) {
            self.git_load_head(cx);
        }
    }

    /// 编辑器没了(重载中 / 变成不可编辑):标记、浮层、注解数据一起清掉;注解的
    /// 开关留着,下一个编辑器到位后自动重跑。
    pub(super) fn git_on_editor_dropped(&mut self) {
        let gutter = &mut self.gutter;
        gutter.head_request = gutter.head_request.wrapping_add(1);
        gutter.diff_request = gutter.diff_request.wrapping_add(1);
        gutter.head = HeadState::Idle;
        gutter.hunks = Rc::default();
        gutter.diff_task = None;
        gutter.popup = None;
        self.annotate.drop_data();
    }

    fn git_load_head(&mut self, cx: &mut Context<Self>) {
        let DocumentSource::Local {
            project_root, path, ..
        } = &self.source
        else {
            return;
        };
        let (root, path) = (project_root.clone(), path.clone());
        self.gutter.head_request = self.gutter.head_request.wrapping_add(1);
        let req = self.gutter.head_request;
        if matches!(self.gutter.head, HeadState::Idle) {
            self.gutter.head = HeadState::Loading;
        }
        cx.spawn(async move |this, cx| {
            let probe = path.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { mt_project::git::head_file(&root, &probe).map(prepare_head) })
                .await;
            let _ = this.update(cx, |view: &mut FileViewer, cx| {
                if view.gutter.head_request != req || view.current_path != path {
                    return;
                }
                view.git_apply_head(outcome, cx);
            });
        })
        .detach();
    }

    fn git_apply_head(&mut self, outcome: anyhow::Result<HeadOutcome>, cx: &mut Context<Self>) {
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                // gutter 是被动展示,取不到就不画;记一笔方便排查
                eprintln!("[git-gutter] 取 HEAD 版本失败: {err:#}");
                HeadOutcome {
                    repo: None,
                    base: None,
                }
            }
        };
        let head_moved = self.gutter.repo.as_ref().map(|r| &r.signature.head)
            != outcome.repo.as_ref().map(|r| &r.signature.head);
        self.gutter.repo = outcome.repo;
        // 浮层里显示的是旧 HEAD 的原文,换代即关
        self.gutter.popup = None;
        self.gutter.head = match outcome.base {
            Some(base) => HeadState::Ready(Rc::new(base)),
            None => {
                // 文件不在 HEAD 里了(或取数失败):上一代的标记一并撤掉
                self.gutter.hunks = Rc::default();
                HeadState::Unavailable
            }
        };
        if self.gutter.repo.is_some() {
            self.git_start_head_poll(cx);
        }
        self.gutter.diff_now = true;
        self.git_schedule_diff(cx);
        // 注解:开着的话,HEAD 动过(提交 / 切分支)或还没有结果时重跑
        self.git_blame_after_head(head_moved, cx);
        cx.notify();
    }

    /// 编辑器内容变了:浮层与注解悬停作废,去抖后重算。
    pub(super) fn git_on_buffer_changed(&mut self, cx: &mut Context<Self>) {
        if self.gutter.popup.take().is_some() {
            cx.notify();
        }
        self.annotate.clear_hover();
        self.git_schedule_diff(cx);
    }

    /// 不去抖、立刻重算一次(注解结果回来时缓冲区已经变了,要马上补行映射)。
    pub(super) fn git_recompute_now(&mut self, cx: &mut Context<Self>) {
        self.gutter.diff_now = true;
        self.git_schedule_diff(cx);
    }

    /// 排一次重算(默认去抖 [`DIFF_DEBOUNCE`];`diff_now` 置位时立刻)。换任务 = 取消旧的。
    pub(super) fn git_schedule_diff(&mut self, cx: &mut Context<Self>) {
        self.gutter.diff_request = self.gutter.diff_request.wrapping_add(1);
        let req = self.gutter.diff_request;
        let delay = !std::mem::take(&mut self.gutter.diff_now);
        self.gutter.diff_task = Some(cx.spawn(async move |this, cx| {
            if delay {
                cx.background_executor().timer(DIFF_DEBOUNCE).await;
            }
            let Ok(Some(input)) =
                this.update(cx, |view: &mut FileViewer, cx| view.git_diff_input(cx))
            else {
                return;
            };
            let output = cx
                .background_executor()
                .spawn(async move { input.compute() })
                .await;
            let _ = this.update(cx, |view: &mut FileViewer, cx| {
                if view.gutter.diff_request == req {
                    view.git_apply_diff(output, cx);
                }
            });
        }));
    }

    fn git_diff_input(&self, cx: &App) -> Option<DiffInput> {
        let editor = self.editor.as_ref()?;
        let head = self.gutter.head_base().map(|base| base.text.clone());
        let blame = self.annotate.diff_input();
        if head.is_none() && blame.is_none() {
            return None;
        }
        Some(DiffInput {
            buffer: editor.read(cx).value(),
            head,
            blame,
        })
    }

    fn git_apply_diff(&mut self, output: DiffOutput, cx: &mut Context<Self>) {
        let hunks = output.hunks.unwrap_or_default();
        if *self.gutter.hunks != hunks {
            // 块的下标变了,开着的浮层指向的可能已不是同一块
            self.gutter.popup = None;
            self.gutter.hunks = Rc::new(hunks);
        }
        if let Some(map) = output.blame {
            self.annotate.apply_line_map(map);
        }
        cx.notify();
    }

    // ── HEAD 指纹 ─────────────────────────────────────────

    fn git_start_head_poll(&mut self, cx: &mut Context<Self>) {
        if self.gutter.poll_task.is_some() {
            return;
        }
        self.gutter.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEAD_POLL).await;
                if this
                    .update(cx, |view: &mut FileViewer, cx| {
                        view.git_check_head(false, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    /// 页签激活:立刻查一次 HEAD 指纹。
    pub(super) fn git_on_activated(&mut self, cx: &mut Context<Self>) {
        self.git_check_head(true, cx);
    }

    /// 保存成功:查一次 HEAD 指纹;注解开着且 blame 之后改过内容的,按落盘内容重跑一次
    /// (行映射是 LCS 对齐,git 自己的归属判定更准,保存是个自然的校准点)。
    pub(super) fn git_on_saved(&mut self, cx: &mut Context<Self>) {
        self.git_check_head(true, cx);
        self.git_blame_after_save(cx);
    }

    /// 查 HEAD 指纹,变了就重取 HEAD 版本。`force` = 不看本页签是不是当前文档。
    fn git_check_head(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.gutter.poll_inflight
            || self.editor.is_none()
            || matches!(self.gutter.head, HeadState::Loading | HeadState::Idle)
        {
            return;
        }
        let Some(repo) = self.gutter.repo.clone() else {
            return;
        };
        if !force && !crate::workbench_area::is_document_active(&self.source, cx) {
            return;
        }
        self.gutter.poll_inflight = true;
        let req = self.gutter.head_request;
        cx.spawn(async move |this, cx| {
            let (root, rel) = (repo.root.clone(), repo.rel_path.clone());
            let signature = cx
                .background_executor()
                .spawn(async move { mt_project::git::head_signature(&root, &rel) })
                .await;
            let _ = this.update(cx, |view: &mut FileViewer, cx| {
                view.gutter.poll_inflight = false;
                if view.gutter.head_request != req {
                    return;
                }
                // 读不到(仓库被删 / 正在被别的 git 进程改写)就等下一拍
                if signature.is_ok_and(|sig| sig != repo.signature) {
                    view.git_load_head(cx);
                }
            });
        })
        .detach();
    }

    // ── 浮层 ─────────────────────────────────────────────

    /// 点了某块的标记。`was_open` 取的是**按下之前**那一帧的状态:浮层自己的
    /// 「点外部关闭」在捕获阶段已经先把它关掉了,这里不能再看实体上的现状,否则点同一块
    /// 标记只会「关掉又打开」。
    fn toggle_change_popup(
        &mut self,
        hunk: usize,
        was_open: bool,
        anchor: Point<Pixels>,
        width: Pixels,
        cx: &mut Context<Self>,
    ) {
        if was_open {
            self.gutter.popup = None;
        } else {
            self.gutter.popup = Some(ChangePopup {
                hunk,
                anchor: Some(anchor),
                width,
                copied: false,
            });
        }
        cx.notify();
    }

    /// 关浮层;真关掉了返回 `true`(Esc 据此决定要不要吃掉这一下)。
    pub(super) fn close_change_popup(&mut self, cx: &mut Context<Self>) -> bool {
        if self.gutter.popup.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// 标记层在 paint 里算出了浮层该在的位置。
    fn place_change_popup(
        &mut self,
        hunk: usize,
        anchor: Option<(Point<Pixels>, Pixels)>,
        cx: &mut Context<Self>,
    ) {
        let Some(popup) = self.gutter.popup.as_mut().filter(|p| p.hunk == hunk) else {
            return;
        };
        popup.anchor = anchor.map(|(at, _)| at);
        if let Some((_, width)) = anchor {
            popup.width = width;
        }
        cx.notify();
    }

    /// 上一处 / 下一处变更:光标挪过去(编辑器自己滚到可见),浮层跟过去。
    pub(super) fn goto_change(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_preview_toggle() && self.preview {
            return;
        }
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let cursor_line = editor.read(cx).cursor_position().line as usize;
        let current = self.gutter.popup.as_ref().map(|p| p.hunk);
        let Some(target) = adjacent_hunk(&self.gutter.hunks, cursor_line, current, forward) else {
            return;
        };
        let hunk = self.gutter.hunks[target].clone();
        editor.update(cx, |state, cx| {
            let last = state.text().lines_len().saturating_sub(1);
            let line = hunk.new_start.min(last) as u32;
            state.set_cursor_position(Position::new(line, 0), window, cx);
        });
        let width = self
            .gutter
            .popup
            .as_ref()
            .map_or(px(POPUP_MIN_WIDTH), |p| p.width);
        self.gutter.popup = Some(ChangePopup {
            hunk: target,
            anchor: None,
            width,
            copied: false,
        });
        cx.notify();
    }

    fn rollback_hunk(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let (Some(hunk), Some(head)) = (
            self.gutter.hunks.get(index).cloned(),
            self.gutter.head_base().cloned(),
        ) else {
            return;
        };
        let Some(old) = head.old_lines(&hunk) else {
            return;
        };
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let text = editor.read(cx).value();
        let (range, replacement) = rollback_edit(&text, &hunk, &old);
        self.gutter.popup = None;
        // 一次原子编辑:可撤销,Change 事件照常标脏;重算不去抖,标记当场消失
        self.gutter.diff_now = true;
        editor.update(cx, |state, cx| {
            state.set_selected_range(range, cx);
            state.replace(replacement, window, cx);
            let line = hunk
                .new_start
                .min(state.text().lines_len().saturating_sub(1)) as u32;
            state.set_cursor_position(Position::new(line, 0), window, cx);
        });
        cx.notify();
    }

    fn copy_hunk_original(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(hunk), Some(head)) = (self.gutter.hunks.get(index), self.gutter.head_base())
        else {
            return;
        };
        let Some(raw) = head.raw_lines.get(hunk.old_range()) else {
            return;
        };
        let text = raw.join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if let Some(popup) = self.gutter.popup.as_mut() {
            popup.copied = true;
        }
        cx.notify();
    }

    /// 「查看完整 diff」:HEAD → 工作区(磁盘上的文件;未保存的改动不在里面)。
    fn show_full_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(head) = self.gutter.head_base() else {
            return;
        };
        let root = head.repo.root.to_string_lossy().into_owned();
        let rel = head.repo.rel_path.clone();
        self.gutter.popup = None;
        cx.notify();
        let store = crate::store::AppStore::global(cx);
        // 打开的是弹窗:等本实体的这次 update 收口再开,免得弹窗回读页签状态时撞租约
        window.defer(cx, move |window, cx| {
            let file = crate::git_diff::DiffFile {
                path: rel,
                side: DiffSide::Working,
                status_label: "M".to_string(),
                hunk_actions: false,
            };
            crate::git_diff::open_file_diff(store, root, vec![file], 0, None, window, cx);
        });
    }

    // ── 渲染 ─────────────────────────────────────────────

    /// 源码态的编辑器:`Editor` 本体 + 左侧注解列(开着时)+ 盖在上面的标记层。
    pub(super) fn render_code_editor(
        &self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let annotations = self.annotation_paint(window);
        let column = annotations.as_ref().map(|a| a.width);
        let popup = self.gutter.popup.as_ref();
        let overlay = OverlaySnapshot {
            editor: editor.clone(),
            view: cx.entity().downgrade(),
            hunks: self.gutter.hunks.clone(),
            active: popup.map(|p| p.hunk),
            active_anchor: popup.and_then(|p| p.anchor),
            annotations,
        };
        div()
            .size_full()
            .relative()
            .flex()
            .overflow_hidden()
            // 注解列:编辑器左侧额外占一列,不盖行号;内容由标记层画
            .when_some(column, |el, width| {
                el.child(div().flex_none().w(width).h_full())
            })
            .child(
                div().flex_1().min_w(px(0.0)).h_full().child(
                    Editor::new(editor)
                        .h_full()
                        .appearance(false)
                        .bordered(false),
                ),
            )
            .child(gutter_overlay(overlay))
            .into_any_element()
    }

    /// 变更小窗(画在整窗之上)。导航后位置还没回填时不画。
    pub(super) fn render_change_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.editor.is_none() || (self.has_preview_toggle() && self.preview) {
            return None;
        }
        let popup = self.gutter.popup.as_ref()?;
        let anchor = popup.anchor?;
        let hunk = self.gutter.hunks.get(popup.hunk)?.clone();
        let old_lines: Vec<String> = self.gutter.head_base()?.old_lines(&hunk)?.to_vec();
        let total = self.gutter.hunks.len();
        let index = popup.hunk;

        let count = match hunk.kind {
            LineChangeKind::Deleted => hunk.old_len,
            _ => hunk.new_len,
        };
        let summary = match hunk.kind {
            LineChangeKind::Added => tr!("gitGutter", "changeAdded", count = count),
            LineChangeKind::Modified => tr!("gitGutter", "changeModified", count = count),
            LineChangeKind::Deleted => tr!("gitGutter", "changeDeleted", count = count),
        };
        let color = kind_color(hunk.kind);
        let line_height = px(13.0 * 1.5);
        let has_old = hunk.old_len > 0;

        let button = |id: &'static str, icon: &'static [Shape], tip: String| {
            div()
                .id(id)
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|el| el.bg(ui::bg_elevated()))
                .tip(tip)
                .child(VectorIcon::new(icon, px(14.0)).ink(ui::text_secondary()))
        };

        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .py(px(6.0))
            .when(has_old, |el| {
                el.border_b_1().border_color(ui::border_subtle())
            })
            .child(
                div()
                    .flex_none()
                    .w(px(3.0))
                    .h(px(14.0))
                    .rounded(px(1.5))
                    .bg(color),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_primary())
                    .child(summary),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(super::editor_font_family())
                    .text_size(ui::font_px(11.5))
                    .text_color(ui::text_muted())
                    .child(format!("{} / {}", index + 1, total)),
            )
            .child(div().flex_1())
            .child(
                button(
                    "git-change-prev",
                    ICON_ARROW_UP,
                    format!(
                        "{} ({})",
                        t("gitGutter", "prevChange"),
                        change_key_label(false)
                    ),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.goto_change(false, window, cx);
                })),
            )
            .child(
                button(
                    "git-change-next",
                    ICON_ARROW_DOWN,
                    format!(
                        "{} ({})",
                        t("gitGutter", "nextChange"),
                        change_key_label(true)
                    ),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.goto_change(true, window, cx);
                })),
            )
            .child(
                button(
                    "git-change-rollback",
                    ICON_ROLLBACK,
                    t("gitGutter", "rollback").to_string(),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.rollback_hunk(index, window, cx);
                })),
            )
            .child(
                button(
                    "git-change-diff",
                    ICON_DIFF,
                    t("gitGutter", "showDiff").to_string(),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.show_full_diff(window, cx);
                })),
            )
            .when(has_old, |el| {
                let (icon, tip) = if popup.copied {
                    (ICON_CHECK, t("gitGutter", "copied"))
                } else {
                    (ICON_COPY, t("gitGutter", "copyOriginal"))
                };
                el.child(
                    button("git-change-copy", icon, tip.to_string()).on_click(cx.listener(
                        move |this, _: &ClickEvent, _window, cx| this.copy_hunk_original(index, cx),
                    )),
                )
            });

        // 旧内容:等宽、删除底色,左边是 HEAD 里的行号
        let number_width = popup_number_width(hunk.old_start + hunk.old_len);
        let body = has_old.then(|| {
            let rows = old_lines.into_iter().enumerate().map(|(i, line)| {
                div()
                    .flex()
                    .h(line_height)
                    .items_center()
                    .bg(ui::diff_del_bg())
                    .child(
                        div()
                            .flex_none()
                            .w(number_width)
                            .pl(px(POPUP_NUMBER_PAD_L))
                            .pr(px(POPUP_NUMBER_PAD_R))
                            .text_right()
                            .text_color(ui::text_muted())
                            .child((hunk.old_start + i + 1).to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(ui::text_primary())
                            .child(SharedString::from(line)),
                    )
            });
            div()
                .id("git-change-popup-lines")
                .max_h(line_height * POPUP_MAX_LINES)
                .overflow_y_scroll()
                .py(px(4.0))
                .font_family(super::editor_font_family())
                .text_size(px(13.0))
                .children(rows)
        });

        let card = div()
            .id("git-change-popup")
            .occlude()
            .w(popup.width)
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_surface())
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                this.close_change_popup(cx);
            }))
            .child(toolbar)
            .children(body);

        Some(
            deferred(
                anchored()
                    .position(anchor)
                    .snap_to_window_with_margin(px(8.0))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

// ─── 后台重算 ─────────────────────────────────────────────────

/// 一次重算的输入:缓冲区 + HEAD 投影(没有就不算标记)+ 注解的基准(没开就不算映射)。
struct DiffInput {
    buffer: SharedString,
    head: Option<SharedString>,
    blame: Option<BlameDiffInput>,
}

struct DiffOutput {
    hunks: Option<Vec<LineHunk>>,
    blame: Option<super::git_blame::LineMap>,
}

impl DiffInput {
    fn compute(self) -> DiffOutput {
        DiffOutput {
            hunks: self
                .head
                .map(|head| mt_project::git::line_hunks(&head, &self.buffer)),
            blame: self.blame.map(|blame| blame.compute(&self.buffer)),
        }
    }
}

// ─── 标记层 ───────────────────────────────────────────────────

/// 标记层的输入快照(render 时拍,paint 时用)。
struct OverlaySnapshot {
    editor: Entity<EditorState>,
    view: WeakEntity<FileViewer>,
    hunks: Rc<Vec<LineHunk>>,
    /// 浮层正开着的那一块
    active: Option<usize>,
    active_anchor: Option<Point<Pixels>>,
    annotations: Option<AnnotationPaint>,
}

/// 盖在编辑器(连同注解列)上的整幅 canvas:paint 阶段读编辑器这一帧的排版来画
/// 标记与注解,并挂上点击 / 悬停监听(见模块注释)。
fn gutter_overlay(snap: OverlaySnapshot) -> impl IntoElement {
    canvas(
        // 整幅一个普通命中框:只用来判断「鼠标在这片区域上、没被浮层挡住」,
        // 不拦任何事件(行为 Normal),编辑器自己的命中照旧
        |bounds, window, _cx| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx| paint_overlay(bounds, hitbox, snap, window, cx),
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn paint_overlay(
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
    snap: OverlaySnapshot,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(geom) = editor_geometry(snap.editor.read(cx)) else {
        return;
    };
    let strip = Bounds::new(
        point(geom.text_left - px(STRIP_WIDTH), bounds.origin.y),
        size(px(STRIP_WIDTH), bounds.size.height),
    );
    let segments = Rc::new(marker_segments(&snap.hunks, &geom.rows));
    let hover_at = |pos: Point<Pixels>, hovered: bool| {
        (hovered && strip.contains(&pos))
            .then(|| segment_at(&segments, pos.y))
            .flatten()
            .map(|seg| seg.hunk)
    };
    let mouse = window.mouse_position();
    let hovered = hover_at(mouse, hitbox.is_hovered(window));

    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        paint_markers(&segments, &geom, strip, hovered, snap.active, window);
        if let Some(annotations) = &snap.annotations {
            annotations.paint(bounds, &geom, &hitbox, snap.view.clone(), window, cx);
        }
    });
    if hovered.is_some() {
        window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
    }

    // 浮层的位置跟着它那一块走(导航后首次落位、窗口缩放、键盘滚动)。滚轮滚动直接关
    if let Some(active) = snap.active {
        let placed = segments
            .iter()
            .find(|seg| seg.hunk == active)
            .map(|seg| popup_placement(seg, &geom));
        let moved = match (placed, snap.active_anchor) {
            (Some((at, _)), Some(old)) => {
                (at.x - old.x).abs() > px(0.5) || (at.y - old.y).abs() > px(0.5)
            }
            (None, None) => false,
            _ => true,
        };
        if moved {
            let view = snap.view.clone();
            let _ = view.update(cx, |view, cx| view.place_change_popup(active, placed, cx));
        }
    }

    let width_for = {
        let geom = geom.clone();
        move |seg: &MarkerSegment| popup_placement(seg, &geom)
    };
    {
        let (segments, hitbox, view) = (segments.clone(), hitbox.clone(), snap.view.clone());
        let active = snap.active;
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
                || !strip.contains(&event.position)
            {
                return;
            }
            let Some(seg) = segment_at(&segments, event.position.y) else {
                return;
            };
            // 吃掉这一下:不让编辑器把光标挪到行首、也不让外层抢焦点
            cx.stop_propagation();
            let (anchor, width) = width_for(&seg);
            let was_open = active == Some(seg.hunk);
            let _ = view.update(cx, |view, cx| {
                view.toggle_change_popup(seg.hunk, was_open, anchor, width, cx)
            });
        });
    }
    {
        let (segments, hitbox, view) = (segments.clone(), hitbox.clone(), snap.view.clone());
        // 悬停高亮与手形光标只在「换了一块」时重画一次
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let now = (hitbox.is_hovered(window) && strip.contains(&event.position))
                .then(|| segment_at(&segments, event.position.y))
                .flatten()
                .map(|seg| seg.hunk);
            if now != hovered {
                let _ = view.update(cx, |_, cx| cx.notify());
            }
        });
    }
    if snap.active.is_some() {
        let (hitbox, view) = (hitbox.clone(), snap.view.clone());
        window.on_mouse_event(move |_: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let _ = view.update(cx, |view, cx| view.close_change_popup(cx));
            }
        });
    }
}

/// 浮层落在该块下方、与正文左缘对齐;宽度跟编辑器正文区走,夹在上下限之间。
fn popup_placement(seg: &MarkerSegment, geom: &EditorGeometry) -> (Point<Pixels>, Pixels) {
    let y = if seg.kind == LineChangeKind::Deleted {
        seg.bottom + px(TRIANGLE_HALF + 2.0)
    } else {
        seg.bottom + px(2.0)
    };
    let room = geom.input.origin.x + geom.input.size.width - geom.text_left - px(16.0);
    let width = room.min(px(POPUP_MAX_WIDTH)).max(px(POPUP_MIN_WIDTH));
    (point(geom.text_left, y), width)
}

fn paint_markers(
    segments: &[MarkerSegment],
    geom: &EditorGeometry,
    strip: Bounds<Pixels>,
    hovered: Option<usize>,
    active: Option<usize>,
    window: &mut Window,
) {
    let bar_x = geom.text_left - px(BAR_GAP + BAR_WIDTH);
    for seg in segments {
        let color = kind_color(seg.kind);
        let highlighted = hovered == Some(seg.hunk) || active == Some(seg.hunk);
        if seg.kind == LineChangeKind::Deleted {
            let y = seg.top;
            if highlighted {
                window.paint_quad(fill(
                    Bounds::new(
                        point(strip.origin.x, y - px(TRIANGLE_HALF + 1.0)),
                        size(strip.size.width, px(TRIANGLE_HALF * 2.0 + 2.0)),
                    ),
                    ui::with_alpha(color, 0.22),
                ));
            }
            let x0 = bar_x - px(1.0);
            let mut path = PathBuilder::fill();
            path.move_to(point(x0, y - px(TRIANGLE_HALF)));
            path.line_to(point(x0 + px(TRIANGLE_WIDTH), y));
            path.line_to(point(x0, y + px(TRIANGLE_HALF)));
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
            continue;
        }
        let height = seg.bottom - seg.top;
        if highlighted {
            window.paint_quad(fill(
                Bounds::new(
                    point(strip.origin.x, seg.top),
                    size(strip.size.width, height),
                ),
                ui::with_alpha(color, 0.22),
            ));
        }
        window.paint_quad(fill(
            Bounds::new(point(bar_x, seg.top), size(px(BAR_WIDTH), height)),
            color,
        ));
    }
}

// ─── 图标(Lucide,24 格,线宽 2) ──────────────────────────────

const LUCIDE_VIEW: (f32, f32, f32) = (0.0, 0.0, 24.0);
const LUCIDE_STROKE: f32 = 2.0 / 24.0;

const fn lucide(d: &'static str) -> Shape {
    Shape::line(
        Ink::Current,
        LUCIDE_STROKE,
        Geom::Path {
            d,
            view: LUCIDE_VIEW,
            even_odd: false,
        },
    )
}

/// arrow-up
const ICON_ARROW_UP: &[Shape] = &[lucide("m5 12 7-7 7 7"), lucide("M12 19V5")];
/// arrow-down
const ICON_ARROW_DOWN: &[Shape] = &[lucide("M12 5v14"), lucide("m19 12-7 7-7-7")];
/// undo-2(回滚)
const ICON_ROLLBACK: &[Shape] = &[
    lucide("M9 14 4 9l5-5"),
    lucide("M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5a5.5 5.5 0 0 1-5.5 5.5H11"),
];
/// file-diff(完整 diff)
const ICON_DIFF: &[Shape] = &[
    lucide("M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"),
    lucide("M9 10h6"),
    lucide("M12 13V7"),
    lucide("M9 17h6"),
];
/// copy
const ICON_COPY: &[Shape] = &[
    lucide("M10 8h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H10a2 2 0 0 1-2-2V10a2 2 0 0 1 2-2z"),
    lucide("M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"),
];
/// check
const ICON_CHECK: &[Shape] = &[lucide("M20 6 9 17l-5-5")];
/// x(注解提示条的关闭钮)
pub(super) const ICON_CLOSE: &[Shape] = &[lucide("M18 6 6 18"), lucide("m6 6 12 12")];

#[cfg(test)]
#[path = "git_gutter_tests.rs"]
mod tests;

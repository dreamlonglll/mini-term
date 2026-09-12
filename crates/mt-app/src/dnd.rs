//! 拖放基建(改造清单 #8)。**内外拖用同一套 gpui API**。
//!
//! # 为什么原版有两套自研鼠标事件而这里一套都不要
//!
//! `src/utils/fileDragState.ts` / `projectDragState.ts` 开头写着根因:Tauri v2 在
//! Windows/WebView2 上开了 `dragDropEnabled` 之后,OLE 原生拖放会吃掉窗口内部的
//! HTML5 `dragover`/`drop`,于是所有内部拖拽只能退回 `mousedown/mousemove/mouseup`
//! 自己实现(含 5px 起拖阈值、幽灵样式、一次性 capture click 抑制、
//! `pointer-events:none` 穿透规则……)。
//!
//! **这个约束在 gpui 侧不存在**:`window.rs` 把平台的 `FileDropEvent` 直接翻译成
//! 内部 drag —— `Entered` 造一个 `AnyDrag{ value: Arc<ExternalPaths> }` 并把事件
//! 改写成 `MouseMove{pressed_button:Left}`,`Submit` 改写成 `MouseUp{Left}`。
//! 所以 `.on_drop::<ExternalPaths>()` 与 `.on_drop::<DragProjectItem>()` 是同一条
//! 分发路径,原版那两套脚手架一行都不必移植。
//!
//! # 三条链路 → 三个载荷
//!
//! | 链路 | 载荷 | 起点 | 落点 |
//! |---|---|---|---|
//! | 项目列表内排序 / 入组 | [`DragProjectItem`] | `project_list.rs` 的行 | 同左 |
//! | 资源管理器 → 加项目 | [`gpui::ExternalPaths`] | 系统 | 项目列表容器 |
//! | 文件树 / 资源管理器 → 终端 | [`DragFilePath`] / `ExternalPaths` | 文件树行 / 系统 | pane 主体 |
//! | 文件树内移动文件/目录 | [`DragFilePath`] | 文件树行 | 目录行 / 文件行(落其父目录)/ 空白处(项目根) |
//! | 终端 tab 移动 / 合并 / 重排 | [`DragPane`] | tab | 另一组的 tab 栏 / 终端区 |
//!
//! # 三条 gpui 硬约束(写代码前必须知道)
//!
//! 1. **`on_drop` 不带位置**(div.rs:976)。落点即「抬手时命中的那个元素」,
//!    before/inside/after 只能由 [`InteractiveElement::on_drag_move`] 提前算好存进
//!    view state,drop 时读 state 定档。本模块的 [`drop_position`] 就是那个判档纯函数。
//! 2. **`on_drag_move` 对**每个**注册过的元素都会触发**,不只是鼠标底下那个
//!    (div.rs:282-305 里只判了 `DispatchPhase::Capture` + 载荷类型,没有 hitbox 判定)。
//!    所以监听里必须自己 `bounds.contains(position)`,否则整列表的行会一起亮。
//!    [`hit_ratio`] 顺带把这道闸做进返回值(不命中 → `None`)。
//! 3. **起拖阈值是 2px 欧氏**(`DRAG_THRESHOLD`,div.rs:47),原版是 5px 曼哈顿。
//!    差异会让拖拽更灵敏;不自己再加一层,**接受并记档**。
//!
//! 另外两件 gpui 白送的事:拖起会重置 `clicked_state`(原版那套「一次性 capture
//! click 抑制」不需要),以及 `on_drag` 的 constructor 返回的实体会跟着鼠标画
//! (原版没有拖影,只让源行变淡 —— 这里两个都做)。
//!
//! # 中途取消与拖拽光标(pane 拖拽批补记)
//!
//! - **Esc 取消**:gpui 没有内建取消,但 [`gpui::App::stop_active_drag`] 是公开的,
//!   配上 `capture_key_down`(捕获相沿根→焦点节点下行,先于终端自己的 `on_key_down`)
//!   就等价于原版 `paneDragState` 里那句 `window.addEventListener('keydown', …, true)`。
//!   挂在 **Workspace 根**(`main.rs`)上、对四条链路一视同仁:此前挂在终端区根容器
//!   上只对 pane 拖拽有效 —— 从文件树起拖时焦点在文件树行上,终端区不在派发路径上。
//!   资源管理器拖进来的 `ExternalPaths` 不走这条:OLE 拖拽期间 Esc 由拖源处理,
//!   gpui 收到 `FileDropEvent::Exited` 时自己清 active_drag。
//! - **grabbing 光标**:Windows 上**拿不到**。`CursorStyle::ClosedHand` 在
//!   gpui 0.2.2 的 `platform/windows/util.rs::load_cursor` 里落进 `_ => IDC_ARROW`,
//!   强行设过去反而从「手形」退化成「箭头」。而 gpui 拖拽期间本来就会把**拖源元素**
//!   的 `mouse_cursor` 提升为全窗口光标(`elements/div.rs:1834`),tab 是
//!   `cursor_pointer` → 整个拖拽过程是手形 —— 那已经是 Windows 上最接近
//!   grabbing 的一档,故**刻意不动**,记档在看板。

use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, Bounds, Context, IntoElement, ParentElement, Pixels, Point, Render,
    SharedString, Styled, Window, div, px,
};
use mt_ui::icons::vector::{Geom, Ink, Shape, VectorIcon};
use mt_ui::icons::{FileIcon, ProjectKind, TechIcon};

use crate::tree::DropZone;
use crate::ui;

// ─── 载荷 ─────────────────────────────────────────────────────

/// 项目列表内部拖拽的载荷(项目行与分组行共用一个类型 —— 两者互为落点,
/// 分成两个类型的话 `on_drag_move::<T>` 的类型闸会把跨类型的那一半挡掉)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragProjectItem {
    pub id: String,
    pub is_group: bool,
}

/// 文件树行的拖拽载荷。原版是模块级单例 `_payload`(只有路径),这里是 gpui 的
/// drag value;**两个落点共用**:终端 pane 主体只看 `path`(写成引号包裹的文本),
/// 文件树自己的行/空白处拿它做**移动**,判「目录不能进自身」要 `is_dir`、
/// 确认框要显示 `name`(压缩链行的显示名是 `src/main/java`,不是路径尾段)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragFilePath {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// 终端 tab(pane)拖拽的载荷,对应原版 `paneDragState.ts` 的 `PaneDragPayload`。
///
/// 带 `project_id` 是**落点的准入闸**:原版 `acceptsPaneDrag` 只接同项目的 pane
/// (跨项目移动 pane 没有意义 —— PTY 的 cwd、SSH 归属都绑在项目上)。
/// 项目切换时另一个项目的终端区压根不在元素树里,这道闸实际是防御性的,但照抄。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragPane {
    pub project_id: String,
    pub pane_id: String,
}

// ─── 落点判档 ─────────────────────────────────────────────────

/// 落在目标行的哪一档。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropPosition {
    Before,
    /// 只有分组行接受(`allowInside=true`);项目行不是容器。
    Inside,
    After,
}

/// 鼠标在行内的纵向比例 → 落点档位。逐字对照 `ProjectList.tsx:520-561`:
/// 允许 inside 的行,中间 50%(0.25..0.75,**开区间**)是 inside,其余按上下半分。
pub fn drop_position(ratio: f32, allow_inside: bool) -> DropPosition {
    if allow_inside && ratio > 0.25 && ratio < 0.75 {
        DropPosition::Inside
    } else if ratio < 0.5 {
        DropPosition::Before
    } else {
        DropPosition::After
    }
}

/// 鼠标落在这个矩形里的纵向比例;不在矩形里 → `None`。
///
/// 见模块注释第 2 条:`on_drag_move` 会打给**所有**注册者,这道闸不能省。
/// 高度为 0 的退化矩形(还没布局出来的那一帧)按不命中处理,避免除零。
pub fn hit_ratio(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
    let height: f32 = bounds.size.height.into();
    if height <= 0.0 {
        return None;
    }
    if !bounds.contains(&position) {
        return None;
    }
    let y: f32 = (position.y - bounds.origin.y).into();
    Some((y / height).clamp(0.0, 1.0))
}

/// before/after 落点 → 插入下标。
///
/// `dragged_idx` 只在被拖项**原本就在同一父级**时才传:先删后插会让它后面的
/// 元素统统前移一格,所以下标要补偿 1(`ProjectList.tsx:592-595` 的那三行)。
pub fn insert_index(target_idx: usize, dragged_idx: Option<usize>, after: bool) -> usize {
    let mut idx = if after { target_idx + 1 } else { target_idx };
    if let Some(dragged_idx) = dragged_idx
        && dragged_idx < idx
    {
        idx -= 1;
    }
    idx
}

// ─── pane 拖拽的两个几何判档 ──────────────────────────────────

/// 终端区落点判档,逐字对照 `PaneGroup.tsx::zoneFromEvent`:
/// 中间那块(x、y 都在 0.25..=0.75)是 [`DropZone::Center`],其余取「离哪条边最近」。
///
/// 不在矩形里 → `None`(见模块注释第 2 条:`on_drag_move` 打给所有注册者,
/// 命中闸必须自己做)。宽/高为 0 的退化矩形同样按不命中处理。
///
/// 并列时的先后与原版一致:TS 那边是 `[left, right, top, bottom]` 上的
/// `Array.prototype.sort`(稳定排序),等价于「取第一个最小值」。
pub fn pane_drop_zone(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<DropZone> {
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    if width <= 0.0 || height <= 0.0 || !bounds.contains(&position) {
        return None;
    }
    // 分母与原版一样兜底到 1(`Math.max(r.width, 1)`)
    let x: f32 = f32::from(position.x - bounds.origin.x) / width.max(1.0);
    let y: f32 = f32::from(position.y - bounds.origin.y) / height.max(1.0);
    if (0.25..=0.75).contains(&x) && (0.25..=0.75).contains(&y) {
        return Some(DropZone::Center);
    }
    let candidates = [
        (DropZone::Left, x),
        (DropZone::Right, 1.0 - x),
        (DropZone::Top, y),
        (DropZone::Bottom, 1.0 - y),
    ];
    let mut best = candidates[0];
    for cand in &candidates[1..] {
        if cand.1 < best.1 {
            best = *cand;
        }
    }
    Some(best.0)
}

/// tab 栏插入位 + 插入指示线的 x,逐字对照 `PaneGroup.tsx::tabDropFromEvent`:
/// 指针落在某个 tab 中线**左侧**就插到它前面,否则继续往右;越过最后一个 tab
/// (落在「+」或右侧控件簇那片空白上)就是末尾。
///
/// `tabs` 是各 tab 的 `(左缘, 右缘)`,按 tab 顺序;坐标与 `x` 同一套(屏幕坐标)。
/// 返回的 x 是**指示线该画在哪**:插到 tab i 前面就贴它左缘,插到末尾就贴最后
/// 一个 tab 的右缘。一个 tab 都没有时退化成 `(0, 0.0)`(原版同,该分支到不了)。
pub fn tab_insert_index(tabs: &[(f32, f32)], x: f32) -> (usize, f32) {
    let mut line_x = 0.0;
    for (i, (left, right)) in tabs.iter().enumerate() {
        if x < left + (right - left) / 2.0 {
            return (i, *left);
        }
        line_x = *right;
    }
    (tabs.len(), line_x)
}

// ─── 路径文本化 ───────────────────────────────────────────────

/// 拖进终端的路径 → 写进 PTY 的文本。
///
/// 规则逐字照抄原版(`useExternalFileDrop.ts:41` 与 `TerminalInstance.tsx:331`):
/// **单引号包裹、不做任何转义、多路径用单个空格 join**。
/// 含单引号的文件名会破 —— 那是原版的已知缺陷,**照抄不修**(修了两侧行为就不一致了)。
pub fn quote_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("'{}'", p.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 单条路径版(文件树那一路)。
pub fn quote_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

// ─── 外部目录拖入项目列表的三态 ───────────────────────────────

/// 拖着外部文件悬停在项目列表上时的提示态(`ProjectList.tsx:1044-1066`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalDropKind {
    /// 至少有一个能加成新项目。
    Valid,
    /// 一个目录都没有(拖的是文件)。
    Forbidden,
    /// 全都是已存在的项目 —— 松手 = 切过去。
    Duplicate,
}

impl ExternalDropKind {
    /// 提示框里那行字的文案 key(`projectList.dragHint.*`)。
    pub fn hint_key(self) -> &'static str {
        match self {
            ExternalDropKind::Valid => "dragHint.valid",
            ExternalDropKind::Forbidden => "dragHint.forbidden",
            ExternalDropKind::Duplicate => "dragHint.duplicate",
        }
    }
}

/// 目录清单 + 现有项目路径 → 三态。
///
/// **入参 `dirs` 必须已经过 `filter_directories` 过滤**(`Path::is_dir` 是同步
/// stat,在网络盘上能卡住主线程,不能在 `on_drag_move` 里逐帧调)。
/// 路径比对走 [`crate::git_worktree::normalize_path`],与
/// `AppStore::find_project_by_path` 同一把尺 —— 提示说「已存在」而落地时又新加
/// 一个,是最难查的那种不一致。
pub fn classify_external(dirs: &[PathBuf], existing_paths: &[String]) -> ExternalDropKind {
    if dirs.is_empty() {
        return ExternalDropKind::Forbidden;
    }
    let existing: Vec<String> = existing_paths
        .iter()
        .map(|p| crate::git_worktree::normalize_path(p))
        .collect();
    let all_dup = dirs.iter().all(|dir| {
        let key = crate::git_worktree::normalize_path(&dir.to_string_lossy());
        existing.iter().any(|e| e == &key)
    });
    if all_dup {
        ExternalDropKind::Duplicate
    } else {
        ExternalDropKind::Valid
    }
}

// ─── 拖影 ─────────────────────────────────────────────────────

/// 「分组 = 空间」的容器图标。原版用 lucide 的 `Boxes`(三个小方块),
/// mt-ui 的图标表里没有对应项 —— 用同一套形状 DSL 在宿主侧拼一份,
/// **不动 mt-ui 的公开 API**。
pub const BOXES_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.09,
        Geom::Rect {
            x: 0.30,
            y: 0.06,
            w: 0.40,
            h: 0.34,
            round: 0.06,
        },
    ),
    Shape::line(
        Ink::Current,
        0.09,
        Geom::Rect {
            x: 0.04,
            y: 0.58,
            w: 0.40,
            h: 0.34,
            round: 0.06,
        },
    ),
    Shape::line(
        Ink::Current,
        0.09,
        Geom::Rect {
            x: 0.56,
            y: 0.58,
            w: 0.40,
            h: 0.34,
            round: 0.06,
        },
    ),
];

/// 终端 pane 拖影的图标。原版没有拖影(只让源 tab 变淡),而 gpui 的 `on_drag`
/// 必须返回一个实体来画 —— 用「窗口 + 提示符」这个通行画法自己拼一个,
/// 与 [`BOXES_SHAPES`] 同理**不动 mt-ui 的公开 API**。
pub const TERMINAL_SHAPES: &[Shape] = &[
    Shape::line(
        Ink::Current,
        0.085,
        Geom::Rect {
            x: 0.06,
            y: 0.14,
            w: 0.88,
            h: 0.72,
            round: 0.10,
        },
    ),
    // 提示符 `>`
    Shape::line(
        Ink::Current,
        0.085,
        Geom::Polyline(&[(0.26, 0.38), (0.44, 0.50), (0.26, 0.62)]),
    ),
    // 光标下划线
    Shape::line(
        Ink::Current,
        0.085,
        Geom::Polyline(&[(0.54, 0.64), (0.76, 0.64)]),
    ),
];

/// 拖影里画哪个图标。`Render` 每帧重跑,所以只能存**数据**不能存 `AnyElement`。
#[derive(Clone, Debug)]
pub enum PreviewIcon {
    /// 项目行:技术栈徽标,认不出退通用目录图标。
    Project(Option<ProjectKind>),
    Group,
    File { name: String, is_dir: bool },
    /// 终端 tab。
    Terminal,
}

/// 跟着鼠标走的拖影。**原版没有这个**(它只让源行 `opacity:0.4`),
/// 但 gpui 的 `on_drag` 本来就要求返回一个实体来画,做成"行名 + 图标"是零成本的加分项;
/// 源行变淡那一半照样保留。
pub struct DragPreview {
    label: SharedString,
    icon: PreviewIcon,
}

impl DragPreview {
    pub fn new(label: impl Into<SharedString>, icon: PreviewIcon) -> Self {
        Self {
            label: label.into(),
            icon,
        }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let icon = match &self.icon {
            PreviewIcon::Project(Some(kind)) => TechIcon::new(*kind).size(px(14.0)).into_any_element(),
            PreviewIcon::Project(None) => FileIcon::folder(false)
                .size(px(14.0))
                .color(ui::color_file())
                .into_any_element(),
            PreviewIcon::Group => VectorIcon::new(BOXES_SHAPES, px(13.0))
                .ink(ui::color_folder())
                .into_any_element(),
            PreviewIcon::File { name, is_dir } => FileIcon::new(name, *is_dir, false)
                .size(px(14.0))
                .into_any_element(),
            PreviewIcon::Terminal => VectorIcon::new(TERMINAL_SHAPES, px(13.0))
                .ink(ui::accent())
                .into_any_element(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .bg(ui::bg_elevated())
            .border_1()
            .border_color(ui::accent())
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_primary())
            .child(icon)
            .child(self.label.clone())
    }
}

/// 拖影实体的统一入口(三处 `on_drag` 的 constructor 都调它)。
pub fn preview(label: impl Into<SharedString>, icon: PreviewIcon, cx: &mut App) -> gpui::Entity<DragPreview> {
    let label = label.into();
    cx.new(|_| DragPreview::new(label, icon))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Size;

    fn bounds(top: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: px(0.0),
                y: px(top),
            },
            size: Size {
                width: px(100.0),
                height: px(height),
            },
        }
    }

    // ─── 判档 ─────────────────────────────────────────────────

    /// 分组行:中间 50% 是 inside,上下各 25% 退回 before/after。
    #[test]
    fn 分组行三档按四分位() {
        assert_eq!(drop_position(0.0, true), DropPosition::Before);
        assert_eq!(drop_position(0.24, true), DropPosition::Before);
        // 0.25 是**闭**边界:原版写的是 `ratio > 0.25`,正好落在线上算 before
        assert_eq!(drop_position(0.25, true), DropPosition::Before);
        assert_eq!(drop_position(0.26, true), DropPosition::Inside);
        assert_eq!(drop_position(0.5, true), DropPosition::Inside);
        assert_eq!(drop_position(0.74, true), DropPosition::Inside);
        assert_eq!(drop_position(0.75, true), DropPosition::After);
        assert_eq!(drop_position(1.0, true), DropPosition::After);
    }

    /// 项目行只有两档 —— 项目不是容器,永远不能往里放。
    #[test]
    fn 项目行只有前后两档() {
        assert_eq!(drop_position(0.0, false), DropPosition::Before);
        assert_eq!(drop_position(0.49, false), DropPosition::Before);
        assert_eq!(drop_position(0.5, false), DropPosition::After);
        assert_eq!(drop_position(0.6, false), DropPosition::After);
        for r in [0.3_f32, 0.5, 0.7] {
            assert_ne!(drop_position(r, false), DropPosition::Inside);
        }
    }

    #[test]
    fn 命中比例只在矩形内有值() {
        let b = bounds(100.0, 20.0);
        assert_eq!(hit_ratio(b, Point { x: px(10.0), y: px(100.0) }), Some(0.0));
        assert_eq!(hit_ratio(b, Point { x: px(10.0), y: px(110.0) }), Some(0.5));
        // 上方 / 下方 / 左右都不算命中
        assert_eq!(hit_ratio(b, Point { x: px(10.0), y: px(99.0) }), None);
        assert_eq!(hit_ratio(b, Point { x: px(10.0), y: px(130.0) }), None);
        assert_eq!(hit_ratio(b, Point { x: px(999.0), y: px(105.0) }), None);
    }

    /// 还没布局出来的那一帧高度是 0,不能除零。
    #[test]
    fn 零高矩形不命中() {
        assert_eq!(
            hit_ratio(bounds(0.0, 0.0), Point { x: px(0.0), y: px(0.0) }),
            None
        );
    }

    // ─── pane 落点判档 ─────────────────────────────────────────

    fn rect(left: f32, top: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: px(left),
                y: px(top),
            },
            size: Size {
                width: px(w),
                height: px(h),
            },
        }
    }

    fn at(b: Bounds<Pixels>, x: f32, y: f32) -> Option<DropZone> {
        pane_drop_zone(b, Point { x: px(x), y: px(y) })
    }

    /// 中间 50%×50% 是 center,四边各 1/4 进深按「离哪条边最近」分。
    #[test]
    fn 终端区四边一四分之一进深其余归中央() {
        let b = rect(0.0, 0.0, 400.0, 200.0);
        assert_eq!(at(b, 200.0, 100.0), Some(DropZone::Center), "正中");
        assert_eq!(at(b, 100.0, 100.0), Some(DropZone::Center), "x=0.25 边界含在内");
        assert_eq!(at(b, 300.0, 100.0), Some(DropZone::Center), "x=0.75 边界含在内");
        // 出了中间那块就按最近边
        assert_eq!(at(b, 10.0, 100.0), Some(DropZone::Left));
        assert_eq!(at(b, 390.0, 100.0), Some(DropZone::Right));
        assert_eq!(at(b, 200.0, 5.0), Some(DropZone::Top));
        assert_eq!(at(b, 200.0, 195.0), Some(DropZone::Bottom));
    }

    /// 角上比的是**归一化**进深,不是像素 —— 扁长的 pane 里左上角靠上更近。
    #[test]
    fn 角落按归一化进深取最近边() {
        let b = rect(0.0, 0.0, 400.0, 200.0);
        // 左上角:x 比例 0.05,y 比例 0.10 → left 更小
        assert_eq!(at(b, 20.0, 20.0), Some(DropZone::Left));
        // 同一个像素距离,但 y 比例更小 → top 赢
        assert_eq!(at(b, 60.0, 20.0), Some(DropZone::Top));
    }

    /// 并列时取先出现的那一档(TS 侧稳定排序的等价物),顺序 left>right>top>bottom。
    #[test]
    fn 进深并列取先出现的档() {
        let b = rect(0.0, 0.0, 100.0, 100.0);
        // 正左上角:四档里 left 与 top 同为 0,取 left
        assert_eq!(at(b, 0.0, 0.0), Some(DropZone::Left));
        // 正右上角:right 与 top 同为 0,取 right
        assert_eq!(at(b, 100.0, 0.0), Some(DropZone::Right));
    }

    #[test]
    fn 不在_pane_矩形里不判档() {
        let b = rect(100.0, 50.0, 200.0, 100.0);
        assert_eq!(at(b, 99.0, 100.0), None);
        assert_eq!(at(b, 200.0, 200.0), None);
        // 还没布局出来的那一帧
        assert_eq!(at(rect(0.0, 0.0, 0.0, 0.0), 0.0, 0.0), None);
    }

    // ─── tab 栏插入位 ─────────────────────────────────────────

    /// 落在 tab 中线左侧插它前面,右侧继续往右;指示线贴对应的边。
    #[test]
    fn tab_插入位按中线左右分() {
        let tabs = [(0.0, 100.0), (100.0, 200.0), (200.0, 300.0)];
        assert_eq!(tab_insert_index(&tabs, 10.0), (0, 0.0));
        assert_eq!(tab_insert_index(&tabs, 49.0), (0, 0.0));
        // 正中线算「右侧」(原版是 `<` 严格小于)
        assert_eq!(tab_insert_index(&tabs, 50.0), (1, 100.0));
        assert_eq!(tab_insert_index(&tabs, 149.0), (1, 100.0));
        assert_eq!(tab_insert_index(&tabs, 150.0), (2, 200.0));
        assert_eq!(tab_insert_index(&tabs, 260.0), (3, 300.0), "越过最后一个 → 末尾");
    }

    /// tab 之间有空隙(gap / 「+」按钮)时指示线贴上一个 tab 的右缘,不会跳回 0。
    #[test]
    fn tab_越过末尾指示线贴最后一个右缘() {
        let tabs = [(0.0, 110.0), (110.0, 230.0)];
        assert_eq!(tab_insert_index(&tabs, 400.0), (2, 230.0));
        assert_eq!(tab_insert_index(&[], 400.0), (0, 0.0), "一个 tab 都没有");
    }

    // ─── 插入下标 ─────────────────────────────────────────────

    #[test]
    fn 插入下标按前后取() {
        assert_eq!(insert_index(3, None, false), 3);
        assert_eq!(insert_index(3, None, true), 4);
    }

    /// 同父级、被拖项在目标之前:先删后插会前移一格,下标要减一。
    #[test]
    fn 同父级前移要补偿() {
        // [a, b, c],把 a 拖到 c 之后 → 目标下标 2,after → 3,a 在前 → 2
        assert_eq!(insert_index(2, Some(0), true), 2);
        // 把 c 拖到 a 之前 → 目标下标 0,before → 0,c 在后不补偿
        assert_eq!(insert_index(0, Some(2), false), 0);
        // 相邻互换:把 a 拖到 b 之后 → 1+1=2,补偿成 1
        assert_eq!(insert_index(1, Some(0), true), 1);
    }

    /// 跨父级不补偿(被拖项不在目标那一层里)。
    #[test]
    fn 跨父级不补偿() {
        assert_eq!(insert_index(0, None, true), 1);
        assert_eq!(insert_index(5, None, false), 5);
    }

    // ─── 路径文本化 ───────────────────────────────────────────

    #[test]
    fn 单条路径单引号包裹() {
        assert_eq!(
            quote_path(Path::new(r"D:\a b\c.txt")),
            r"'D:\a b\c.txt'".to_string()
        );
    }

    #[test]
    fn 多条路径空格连接() {
        let paths = vec![PathBuf::from(r"C:\x"), PathBuf::from(r"C:\y z")];
        assert_eq!(quote_paths(&paths), r"'C:\x' 'C:\y z'".to_string());
    }

    /// 含单引号的文件名会破 —— 原版已知缺陷,照抄不修,用例把这条行为钉住,
    /// 免得后来人"顺手修一下"导致两侧不一致。
    #[test]
    fn 单引号不转义与原版一致() {
        assert_eq!(quote_path(Path::new("it's.txt")), "'it's.txt'".to_string());
    }

    #[test]
    fn 空清单空串() {
        assert_eq!(quote_paths(&[]), String::new());
    }

    // ─── 外部拖入三态 ─────────────────────────────────────────

    #[test]
    fn 没有目录就是禁止() {
        assert_eq!(classify_external(&[], &[]), ExternalDropKind::Forbidden);
    }

    #[test]
    fn 全是已有项目就是重复() {
        let dirs = vec![PathBuf::from(r"D:\Git\a"), PathBuf::from(r"D:\Git\b")];
        let existing = vec![r"D:\Git\a".to_string(), r"D:\Git\b".to_string()];
        assert_eq!(
            classify_external(&dirs, &existing),
            ExternalDropKind::Duplicate
        );
    }

    /// 分隔符 / 大小写 / 尾斜杠都不该让「已存在」判成「新项目」。
    #[test]
    fn 重复判定走路径归一() {
        let dirs = vec![PathBuf::from(r"D:\Git\A")];
        let existing = vec!["d:/git/a/".to_string()];
        assert_eq!(
            classify_external(&dirs, &existing),
            ExternalDropKind::Duplicate
        );
    }

    #[test]
    fn 只要有一个新的就是有效() {
        let dirs = vec![PathBuf::from(r"D:\Git\a"), PathBuf::from(r"D:\Git\新")];
        let existing = vec![r"D:\Git\a".to_string()];
        assert_eq!(classify_external(&dirs, &existing), ExternalDropKind::Valid);
    }

    #[test]
    fn 三态各有各的文案() {
        let keys = [
            ExternalDropKind::Valid.hint_key(),
            ExternalDropKind::Forbidden.hint_key(),
            ExternalDropKind::Duplicate.hint_key(),
        ];
        assert_eq!(
            keys,
            ["dragHint.valid", "dragHint.forbidden", "dragHint.duplicate"]
        );
        // 三条互不相同 —— 复制粘贴写错 key 的话这里红
        assert_eq!(keys.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    }
}

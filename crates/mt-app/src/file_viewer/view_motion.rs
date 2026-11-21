//! 文件页的三处动效里能拆成纯数的那部分(视图接线在 `mod.rs` / `preview.rs` /
//! `git_gutter.rs`):
//!
//! - **「预览 / 源码」段控件**:选中底块是一块滑块([`slider_span`]),在两段之间按
//!   [`mt_ui::motion::TAB_INDICATOR`] 滑;字色跟着滑块盖住这一段的比例过渡([`coverage`] +
//!   [`mix`]),滑块还没到的那一段不会先变成「底色字」而看不见;
//! - **内容左右推**:预览在左、源码在右,两层首尾相接成一条带,带的位置 `pos`(0 = 预览
//!   在台上,1 = 源码在台上)按 [`mt_ui::motion::PANEL_SWAP`] 补间 —— 半路反向点只是让带
//!   从当前位置掉头,不跳变。画法借 [`crate::motion_kit::push_pair`],[`strip_push`] 把带的
//!   位置换成它要的「进度 + 方向」;
//! - **md 目录栏**:正文宽度一次跳到位,只让目录栏内容在裁剪层里横向滑([`outline_offset`])。

use gpui::{Hsla, Rgba};
use mt_ui::motion::{PANEL_SWAP, TAB_INDICATOR, ValueTween};

/// 带的位置:预览在台上。
pub(super) const AT_PREVIEW: f32 = 0.0;
/// 带的位置:源码在台上。
pub(super) const AT_SOURCE: f32 = 1.0;

/// 「预览 / 源码」对应的带位置 / 滑块位置。
pub(super) fn position(preview: bool) -> f32 {
    if preview { AT_PREVIEW } else { AT_SOURCE }
}

/// 停在 `preview` 那一侧的内容带(规格 [`PANEL_SWAP`])。
pub(super) fn strip_tween(preview: bool) -> ValueTween {
    ValueTween::settled(position(preview), PANEL_SWAP)
}

/// 停在 `preview` 那一段的滑块(规格 [`TAB_INDICATOR`])。
pub(super) fn seg_tween(preview: bool) -> ValueTween {
    ValueTween::settled(position(preview), TAB_INDICATOR)
}

/// md 目录栏的开合补间(规格 [`PANEL_SWAP`]),停在 `open` 那一端。
pub(super) fn outline_tween(open: bool) -> ValueTween {
    ValueTween::settled(if open { 1.0 } else { 0.0 }, PANEL_SWAP)
}

/// 段控件滑块这一帧的 `(左缘, 宽)`。`preview` / `source` 是两段各自的 `(左缘, 宽)`
/// (同一参照系),`pos` 是滑块位置(0 = 盖住预览段,1 = 盖住源码段,已缓动)。
/// 两段宽度不等(英文界面「Preview」比「Source」宽)时宽度也跟着插值。端点精确。
pub(super) fn slider_span(preview: (f32, f32), source: (f32, f32), pos: f32) -> (f32, f32) {
    let p = pos.clamp(0.0, 1.0);
    if p <= 0.0 {
        return preview;
    }
    if p >= 1.0 {
        return source;
    }
    (
        preview.0 + (source.0 - preview.0) * p,
        preview.1 + (source.1 - preview.1) * p,
    )
}

/// 滑块 `slider` 盖住段 `seg` 的比例(0..=1,按段宽算)。两者都是 `(左缘, 宽)`。
pub(super) fn coverage(slider: (f32, f32), seg: (f32, f32)) -> f32 {
    if seg.1 <= 0.0 {
        return 0.0;
    }
    let left = slider.0.max(seg.0);
    let right = (slider.0 + slider.1).min(seg.0 + seg.1);
    ((right - left) / seg.1).clamp(0.0, 1.0)
}

/// `a` → `b` 按 `t` 线性混色(在 RGB 空间混,灰阶之间不会绕色相)。
pub(super) fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    let (a, b) = (a.to_rgb(), b.to_rgb());
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    }
    .into()
}

/// 带的位置 `pos` → [`crate::motion_kit::push_pair`] 的 `(进度, 方向)`。`to_source` = 这一场
/// 往源码切(源码是进场层)。
///
/// `push_pair` 里进场层在 `dir × (1 − 进度)`、出场层在 `−dir × 进度`(容器宽度的分数)。
/// 往源码切:源码(进场)从右进,`dir = +1`、进度 = `pos`;往预览切:预览(进场)从左进,
/// `dir = −1`、进度 = `1 − pos`。两种情况下预览层都在 `−pos`、源码层都在 `1 − pos` ——
/// 同一条带,所以半路掉头不跳。
pub(super) fn strip_push(pos: f32, to_source: bool) -> (f32, f32) {
    let pos = pos.clamp(0.0, 1.0);
    if to_source {
        (pos, 1.0)
    } else {
        (1.0 - pos, -1.0)
    }
}

/// 目录栏内容这一帧的横向偏移:`openness`(0 = 收起,1 = 展开,已缓动)下从左缘外
/// 整幅 `width` 处滑到 0。终点精确归零。
pub(super) fn outline_offset(openness: f32, width: f32) -> f32 {
    let p = openness.clamp(0.0, 1.0);
    if p >= 1.0 { 0.0 } else { -(1.0 - p) * width }
}

#[cfg(test)]
#[path = "view_motion_tests.rs"]
mod tests;

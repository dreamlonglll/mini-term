//! 单行省略截断的文本元素([`TruncatedText`])。
//!
//! # 为什么不能用 `div().truncate()`
//!
//! gpui 的 `truncate()` = `overflow_hidden + whitespace_nowrap + text_ellipsis`,
//! 截断发生在文本的**测量闭包**里,而这个闭包有一个按 `wrap_width` 命中的缓存
//! (`gpui/src/elements/text.rs` `TextLayout::layout`):
//!
//! - nowrap 时 `wrap_width` 恒为 `None`,缓存条件 `wrap_width.is_none()` 永远为真,
//!   于是**第一次测量的结果一锤定音**。flex 项的第一次测量是 MaxContent(算
//!   flex-basis),量出整段全宽;收缩后再按定宽量一次直接吃缓存,`truncate_line`
//!   根本没跑 —— 结果是只裁剪、没有「…」(分支名尾巴上剩半个字母那种)。
//! - 不 nowrap、改 `line_clamp(1)` 也不行:taffy 多趟布局里有一趟按 `flex_1`
//!   容器的基准宽 **0** 去量子树(算 hypothetical cross size),`truncate_line(…, 0)`
//!   得到的「…」被缓存,后面 MaxContent 那趟又因为 `wrap_width.is_none()` 命中
//!   它,元素就只剩一个「…」或干脆尺寸为 0。
//!
//! 只有 `flex_1`(basis 0,第一次测量就是定宽)的项碰巧能出省略号,宽度随内容走
//! 的项(胶囊、徽章、标题)全中招。
//!
//! # 这里的做法
//!
//! 测量只报**自然宽度**(整段整形一次),flex 想怎么收缩随它;`prepaint` 拿到
//! 最终 bounds 后,放不下才按 bounds 宽度 `truncate_line` + 重新整形;`paint`
//! 画整形后的那一行。整形不跨帧缓存 —— 每帧一次 `shape_line`,这类文本都是
//! 一行几十个字,便宜。
//!
//! 字号 / 字体 / 颜色一律继承 `window.text_style()`,与普通文本子节点一致,
//! 所以照旧在外层 div 上 `text_size` / `text_color`。
//!
//! # 设备像素取整的坑(125% / 150% 缩放下末字变「…」)
//!
//! gpui 给 taffy 开了整像素取整(`taffy.enable_rounding()`),最终尺寸在**设备像素**
//! 上按 `round(x+w) − round(x)` 算。自然宽度 69 逻辑 px 在 125% 下是 86.25 设备 px,
//! 取整成 86 → 除回来 68.8 逻辑 px,盒子比文字短了 0.2px;`prepaint` 若精确比较就会
//! 走进截断分支,而 DirectWrite 给的字宽全是整数,`truncate_line` 里
//! `width.floor() > truncate_width` 的亚像素容忍一点用没有 → 明明放得下,末字却被
//! 换成「…」。100% 缩放下所有宽度都是整数、取整不缩水,所以只在高 DPI 屏上看得到。
//!
//! 对策两道:测量宽度先**向上取整到设备像素**再上报(整数设备宽经 `round(x+w) − round(x)`
//! 恒等于自身,盒子绝不比文字短),`prepaint` 比较时再留 1 个设备像素容差兜 f32
//! 一来一回的噪声。

use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ShapedLine, SharedString, Size, Style, TextRun, Window, px,
    size,
};

/// 单行文本,放不下时尾部换成「…」。
pub struct TruncatedText {
    text: SharedString,
}

impl TruncatedText {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

/// 省略号。与 gpui `text_ellipsis()` 用的同一个字符。
const ELLIPSIS: &str = "…";

/// `request_layout` 里算好、给后两个阶段用的东西。
pub struct Measured {
    font_size: Pixels,
    line_height: Pixels,
    runs: Vec<TextRun>,
    /// 整段文本的整形结果(自然宽度)。
    full: ShapedLine,
}

impl IntoElement for TruncatedText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TruncatedText {
    type RequestLayoutState = Measured;
    /// 放不下时按最终宽度截断后的整形结果;`None` = 放得下,画 `full`。
    type PrepaintState = Option<ShapedLine>;

    fn id(&self) -> Option<ElementId> {
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
        _cx: &mut App,
    ) -> (LayoutId, Measured) {
        let text_style = window.text_style();
        let rem = window.rem_size();
        let font_size = text_style.font_size.to_pixels(rem);
        let line_height = text_style.line_height.to_pixels(font_size.into(), rem);
        let runs = vec![text_style.to_run(self.text.len())];
        let full = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);
        // 宽度向上取整到设备像素,否则 taffy 取整后盒子可能比文字短(见模块注释)
        let natural: Size<Pixels> = size(
            snap_up_to_device_px(full.width, window.scale_factor()),
            line_height,
        );

        let mut style = Style::default();
        // 能在 flex 里收缩到 0:截断的前提就是允许被压
        style.min_size.width = px(0.0).into();
        style.flex_shrink = 1.0;
        let layout_id = window.request_measured_layout(style, move |known, available, _, _| {
            // 已知宽度照收;定宽 available(块级父节点)按它封顶;
            // Min/MaxContent 一律报自然宽 —— 收不收缩交给 flex,不在这里猜
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(w) => natural.width.min(w),
                _ => natural.width,
            });
            size(width, known.height.unwrap_or(natural.height))
        });
        (
            layout_id,
            Measured {
                font_size,
                line_height,
                runs,
                full,
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measured: &mut Measured,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<ShapedLine> {
        let width = bounds.size.width;
        if fits(measured.full.width, width, window.scale_factor()) {
            return None;
        }
        let font = window.text_style().font();
        let mut wrapper = window.text_system().line_wrapper(font, measured.font_size);
        let mut runs = measured.runs.clone();
        let truncated = wrapper.truncate_line(self.text.clone(), width, ELLIPSIS, &mut runs);
        Some(
            window
                .text_system()
                .shape_line(truncated, measured.font_size, &runs, None),
        )
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measured: &mut Measured,
        truncated: &mut Option<ShapedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line = truncated.as_ref().unwrap_or(&measured.full);
        // 整形失败只会少画一行字,不值得 panic
        let _ = line.paint(bounds.origin, measured.line_height, window, cx);
    }
}

/// 逻辑像素宽度向上取整到整数个设备像素(结果仍以逻辑像素表示)。
///
/// taffy 的 `round(x+w) − round(x)` 对整数 `w` 恒等于 `w`,取整后的盒子才不会比
/// 文字窄;最多多出不到 1 个设备像素的右侧留白,文字左对齐画,看不出来。
fn snap_up_to_device_px(width: Pixels, scale_factor: f32) -> Pixels {
    px((f32::from(width) * scale_factor).ceil() / scale_factor)
}

/// 「放得下」的判定:留 1 个设备像素容差,吞掉测量值 ×scale → taffy → ÷scale
/// 一来一回的 f32 噪声。真被 flex 压窄不到 1 个设备像素时也按放得下处理 ——
/// 溢出量肉眼不可见,却省掉一个只剩「…」的假截断。
fn fits(full_width: Pixels, box_width: Pixels, scale_factor: f32) -> bool {
    f32::from(full_width) <= f32::from(box_width) + 1.0 / scale_factor
}

#[cfg(test)]
mod tests {
    use super::*;

    /// taffy `round_layout` 对单个节点的宽度取整(`compute/mod.rs`),`x` 为该节点
    /// 在设备像素上的累计横坐标。
    fn taffy_rounded_width(x: f32, w: f32) -> f32 {
        (x + w).round() - x.round()
    }

    /// 探针实测:Fira Code 13px 下「gitlab-旧」自然宽 69 逻辑 px,125% 缩放时
    /// 86.25 设备 px 被取整成 86 → 68.8 逻辑 px,精确比较就会误判放不下。
    #[test]
    fn 取整后的盒子不再比文字窄() {
        for &scale in &[1.0f32, 1.25, 1.5, 1.75, 2.0] {
            for natural in [42.0f32, 63.0, 64.0, 69.0, 103.0, 112.0, 113.0, 139.0, 147.0] {
                let reported = f32::from(snap_up_to_device_px(px(natural), scale)) * scale;
                // 上报给 taffy 的是整数个设备像素
                assert!(
                    (reported - reported.round()).abs() < 1e-3,
                    "scale={scale} natural={natural} reported={reported}"
                );
                // 不论节点落在哪个亚像素位置,取整后的盒子都放得下整段文字
                for i in 0..20 {
                    let x = 37.0 + i as f32 * 0.05;
                    let boxed = taffy_rounded_width(x, reported) / scale;
                    assert!(
                        fits(px(natural), px(boxed), scale),
                        "scale={scale} natural={natural} x={x} boxed={boxed}"
                    );
                }
            }
        }
    }

    #[test]
    fn 不取整的旧口径在_125_缩放下确实会缩水() {
        // 复现修前的病灶:69 × 1.25 = 86.25,x 落在整数上时盒子只剩 86 设备 px
        let boxed = taffy_rounded_width(40.0, 69.0 * 1.25) / 1.25;
        assert!(boxed < 69.0, "boxed={boxed}");
        // 精确比较会判放不下;带容差的判定则不会
        assert!(!(69.0 <= boxed));
        assert!(fits(px(69.0), px(boxed), 1.25));
    }

    #[test]
    fn 整数设备宽不被抬高() {
        // 64 × 1.25 = 80 已是整数,不该被 ceil 多推 1 像素
        assert_eq!(f32::from(snap_up_to_device_px(px(64.0), 1.25)), 64.0);
        assert_eq!(f32::from(snap_up_to_device_px(px(112.0), 1.5)), 112.0);
        assert_eq!(f32::from(snap_up_to_device_px(px(69.0), 1.0)), 69.0);
    }

    #[test]
    fn 真放不下时仍判放不下() {
        // 差 1 个逻辑像素以上(超过 1 设备像素容差)必须截断
        assert!(!fits(px(69.0), px(67.5), 1.25));
        assert!(!fits(px(69.0), px(40.0), 1.5));
        // 差不到 1 个设备像素:按放得下(溢出肉眼不可见)
        assert!(fits(px(69.0), px(68.4), 1.25));
        assert!(!fits(px(69.0), px(68.1), 1.25));
    }
}

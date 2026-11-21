use super::*;
use crate::motion_kit::push_offsets;

#[test]
fn 滑块在两段之间插值且端点精确() {
    let (preview, source) = ((0.0, 52.0), (52.0, 44.0));
    assert_eq!(slider_span(preview, source, 0.0), preview);
    assert_eq!(slider_span(preview, source, 1.0), source);
    assert_eq!(slider_span(preview, source, -0.5), preview, "越界钳住");
    assert_eq!(slider_span(preview, source, 1.5), source);
    let (left, width) = slider_span(preview, source, 0.5);
    assert!((left - 26.0).abs() < 1e-4 && (width - 48.0).abs() < 1e-4);
    // 全程:左缘单调右移,不越过两端
    let mut prev = preview.0;
    for i in 1..=10 {
        let (left, _) = slider_span(preview, source, i as f32 / 10.0);
        assert!(left >= prev && left <= source.0);
        prev = left;
    }
}

#[test]
fn 字色按滑块盖住的比例过渡() {
    let seg = (52.0, 44.0);
    assert_eq!(coverage((0.0, 52.0), seg), 0.0, "滑块还在另一段");
    assert_eq!(coverage((52.0, 44.0), seg), 1.0, "滑块到位");
    assert!((coverage((74.0, 44.0), seg) - 0.5).abs() < 1e-4);
    assert_eq!(coverage((0.0, 10.0), (5.0, 0.0)), 0.0, "零宽段不除零");
    // 混色端点就是两端的颜色
    let (a, b) = (
        gpui::hsla(0.0, 0.0, 0.2, 1.0),
        gpui::hsla(0.0, 0.0, 0.9, 1.0),
    );
    assert_eq!(mix(a, b, 0.0), a);
    assert_eq!(mix(a, b, 1.0), b);
    let mid = mix(a, b, 0.5);
    assert!(mid.l > a.l && mid.l < b.l);
}

#[test]
fn 预览源码是一条带_两个方向同一位置() {
    for i in 0..=20 {
        let pos = i as f32 / 20.0;
        // 往源码切:源码是进场层;往预览切:预览是进场层
        let (p, dir) = strip_push(pos, true);
        let (source_at, preview_at) = push_offsets(dir, p);
        let (q, back) = strip_push(pos, false);
        let (preview_back, source_back) = push_offsets(back, q);
        assert!((preview_at - -pos).abs() < 1e-5, "预览层在 −pos");
        assert!((source_at - (1.0 - pos)).abs() < 1e-5, "源码层在 1 − pos");
        assert!(
            (preview_back - preview_at).abs() < 1e-5,
            "掉头不跳:预览层同位"
        );
        assert!(
            (source_back - source_at).abs() < 1e-5,
            "掉头不跳:源码层同位"
        );
    }
    // 端点:到源码时源码精确在 0,到预览时预览精确在 0
    let (p, dir) = strip_push(AT_SOURCE, true);
    assert_eq!(push_offsets(dir, p).0, 0.0);
    let (p, dir) = strip_push(AT_PREVIEW, false);
    assert_eq!(push_offsets(dir, p).0, 0.0);
}

#[test]
fn 目录栏从左缘外滑入且终点归零() {
    assert_eq!(outline_offset(0.0, 248.0), -248.0, "收起时整幅在左缘外");
    assert_eq!(outline_offset(1.0, 248.0), 0.0);
    assert_eq!(outline_offset(1.3, 248.0), 0.0);
    assert_eq!(outline_offset(-0.2, 248.0), -248.0);
    assert!((outline_offset(0.5, 248.0) - -124.0).abs() < 1e-4);
}

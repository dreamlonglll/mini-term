use super::*;

/// 中栏推拉轨道宽度与目标宽度夹紧:展开从 0 长到目标宽、折叠从目标宽收回 0,
/// 端点精确;右抽屉与提交详情栏进出场端点精确,带着详情栏关抽屉整体滑出视口右缘。
#[test]
fn 中栏与详情栏的位移端点精确() {
    assert_eq!(middle_visible_width(263.0, true, 0.0), 0.0);
    assert_eq!(middle_visible_width(263.0, true, 1.0), 263.0);
    assert!((middle_visible_width(263.0, true, 0.25) - 65.75).abs() < 1e-4);
    assert_eq!(middle_visible_width(263.0, false, 0.0), 263.0);
    assert_eq!(middle_visible_width(263.0, false, 1.0), 0.0);
    assert_eq!(middle_visible_width(263.0, false, 2.0), 0.0, "越界钳住");
    // 目标宽度夹紧:动画轨道与 h_resizable 共用唯一口径,窄窗时给右侧留出安全余量
    assert_eq!(middle_target_width(263.0, 1920.0), 263.0);
    assert_eq!(middle_target_width(90.0, 1920.0), MIDDLE_MIN_WIDTH);
    assert_eq!(middle_target_width(999.0, 1920.0), MIDDLE_MAX_WIDTH);
    assert_eq!(
        middle_target_width(600.0, 500.0),
        500.0 - activity_bar::WIDTH - MIDDLE_RIGHT_RESERVE,
        "窗口窄时上限顺应可用宽度"
    );
    assert_eq!(detail_slide_left(320.0, 1.0), 0.0);
    assert_eq!(detail_slide_left(320.0, 0.5), 160.0);
    assert_eq!(detail_slide_left(320.0, 0.0), 320.0);
    assert_eq!(detail_slide_left(320.0, 1.5), 0.0, "越界钳住");
    // 单抽屉滑出
    assert_eq!(drawer_slide_right(360.0, 0.0, 1.0), 0.0);
    assert_eq!(drawer_slide_right(360.0, 0.0, 0.0), -360.0);
    // 带着 420px 提交详情栏关闭抽屉:终点必须把 360 + 420 整块送出视口右缘
    assert_eq!(drawer_slide_right(360.0, 420.0, 1.0), 0.0);
    assert_eq!(drawer_slide_right(360.0, 420.0, 0.5), -390.0);
    assert_eq!(drawer_slide_right(360.0, 420.0, 0.0), -780.0);
}

/// 中栏半路掉头不跳:收到一半又展开(或反过来),新一场从 `1 − 进度` 起步,
/// 轨道此刻的宽度与掉头前一模一样(见 [`MiddleMotion`])。
#[test]
fn 中栏开合半路掉头从当前位置接着走() {
    for entering in [true, false] {
        for i in 0..=20 {
            let p = i as f32 / 20.0;
            let before = middle_visible_width(320.0, entering, p);
            let after = middle_visible_width(320.0, !entering, motion_kit::reversed_progress(p));
            assert!((before - after).abs() < 1e-4, "entering={entering} p={p}");
        }
    }
    // 起步进度由 `resume_band` 接上:建出来那一刻就在起步值上
    let now = std::time::Instant::now();
    let band = motion_kit::resume_band(0.6, mt_ui::motion::SIDEBAR_TOGGLE, now);
    assert!((band.value_at(now) - 0.6).abs() < 1e-6);
    assert_eq!(band.target(), 1.0);
}

/// 抽屉换面板:往段控件右边那项(Git)切新面板从右进,反之从左;滑块停在各自那一半。
#[test]
fn 抽屉换面板的方向与滑块位置() {
    assert!(drawer_panel_order(DrawerPanel::Sessions, DrawerPanel::Git) > 0.0);
    assert!(drawer_panel_order(DrawerPanel::Git, DrawerPanel::Sessions) < 0.0);
    assert_eq!(drawer_panel_order(DrawerPanel::Git, DrawerPanel::Git), 0.0);
    assert_eq!(drawer_tab_left(DrawerPanel::Sessions), 0.0);
    assert_eq!(drawer_tab_left(DrawerPanel::Git), 0.5);
}

/// 三栏宿主必须能收缩到内容 min-content 以下,理由与实测数据见 [`columns_host`]。
/// 布局本身要真窗口才能跑(本 crate 不开 gpui 的 test-support),这里钉的是
/// 那条决定性的样式:哪天有人把 `min_w(0)` 当冗余删了,这条先红。
#[test]
fn 三栏宿主能收缩到内容最小宽度以下() {
    let mut host = columns_host();
    let style = host.style();
    assert_eq!(
        style.min_size.width,
        Some(px(0.0).into()),
        "min_w(0) 不能省"
    );
    assert_eq!(style.flex_grow, Some(1.0), "仍然吃满 body 的剩余宽度");
}

/// 抽屉挤不挤页签区:终端页、抽屉没开(含退场驻留)一律 0 —— 终端宽度一像素不动;
/// 文件 / diff 页上让出抽屉连同详情栏的宽。
#[test]
fn 只在文件页上挤占页签区() {
    assert_eq!(
        drawer_squeeze(true, true, 360.0, 1200.0),
        0.0,
        "终端页照旧悬浮"
    );
    assert_eq!(
        drawer_squeeze(false, false, 360.0, 1200.0),
        0.0,
        "抽屉收着 / 正在退场"
    );
    assert_eq!(drawer_squeeze(true, false, 360.0, 1200.0), 0.0);
    assert_eq!(drawer_squeeze(false, true, 360.0, 1200.0), 360.0);
    // 抽屉 + 详情栏一起让
    assert_eq!(drawer_squeeze(false, true, 360.0 + 420.0, 1600.0), 780.0);
}

/// 页签区至少留 [`SQUEEZE_MIN_DOC_W`]:放不下时只让到那条底,多出来的照旧盖着;
/// 那一栏本身比底还窄时不让(整栏都被盖住,与挤占前一样),绝不出负数。
#[test]
fn 挤占给页签区留底() {
    let panel = 900.0;
    assert_eq!(
        drawer_squeeze(false, true, 780.0, panel),
        panel - SQUEEZE_MIN_DOC_W
    );
    assert_eq!(
        drawer_squeeze(false, true, 360.0, panel),
        360.0,
        "放得下就全让"
    );
    assert_eq!(drawer_squeeze(false, true, 360.0, 200.0), 0.0);
    assert_eq!(drawer_squeeze(false, true, 0.0, 1200.0), 0.0);
}

/// 提交详情栏的宽:想要多宽给多宽,窗口窄时先给左侧留出 [`GIT_DETAIL_TERMINAL_ROOM`],
/// 但不窄于 [`GIT_DETAIL_FLOOR_W`](宁可压住一截,也别挤成一条缝)。挤占页签区也按这个宽。
#[test]
fn 详情栏宽度让一让但有底() {
    assert_eq!(detail_layer_width(420.0, 1920.0, 360.0), 420.0);
    assert_eq!(
        detail_layer_width(900.0, 1280.0, 360.0),
        1280.0 - 360.0 - GIT_DETAIL_TERMINAL_ROOM
    );
    assert_eq!(detail_layer_width(420.0, 700.0, 360.0), GIT_DETAIL_FLOOR_W);
}

/// 左缘拖拽:往左 = 变宽、往右 = 变窄,按住不放时来回拖都以按下那一刻为基准;
/// 钳到上下限后鼠标接着动不报变化(不白画整窗帧)。issue #107 的场景:1080 宽的窗口、
/// 抽屉停在旧上限 720 —— 往右拖窄必须走得通,往左也能接着拖宽(上限只剩窗口)。
#[test]
fn 左缘拖拽两个方向都能改宽且钳住后不报变化() {
    let (min, max) = (store::RIGHT_DRAWER_MIN_WIDTH, drawer_max_width(1080.0));
    let mut drag = DrawerDrag::new(px(360.0), 720.0);

    // 往右拖进抽屉里 = 变窄
    assert!(drag.track(px(460.0), min, max));
    assert_eq!(drag.width, 620.0);

    // 往左越过旧上限 720 接着变宽
    assert!(drag.track(px(200.0), min, max));
    assert_eq!(drag.width, 880.0);

    // 往左拖过头钳到窗口上限(左侧活动栏边上),再往左不报变化
    assert!(drag.track(px(0.0), min, max));
    assert_eq!(drag.width, max);
    assert!(!drag.track(px(-50.0), min, max));

    // 往右拖过头钳到下限,再往右不报变化
    assert!(drag.track(px(2000.0), min, max));
    assert_eq!(drag.width, min);
    assert!(!drag.track(px(2100.0), min, max));
}

/// 抽屉没有固定上限:最宽到左侧活动栏边上;窗口窄到放不下下限时按下限给。
#[test]
fn 抽屉上限只受窗口限制() {
    assert_eq!(
        drawer_max_width(1080.0),
        1080.0 - activity_bar::WIDTH as f64
    );
    assert_eq!(
        drawer_max_width(3840.0),
        3840.0 - activity_bar::WIDTH as f64
    );
    assert_eq!(drawer_max_width(200.0), store::RIGHT_DRAWER_MIN_WIDTH);
}

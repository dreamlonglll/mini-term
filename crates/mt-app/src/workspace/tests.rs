use super::*;

/// 中栏内容层:滑入从整幅在左推到 0、滑出从 0 推到整幅在左,端点精确(残留零点几会让
/// 中栏内容永远错位);提交详情栏退场从 0 收到整栏宽。
#[test]
fn 中栏与详情栏的位移端点精确() {
    assert_eq!(middle_content_left(true, 0.0), -1.0);
    assert_eq!(middle_content_left(true, 1.0), 0.0);
    assert_eq!(middle_content_left(true, 0.25), -0.75);
    assert_eq!(middle_content_left(false, 0.0), 0.0);
    assert_eq!(middle_content_left(false, 1.0), -1.0);
    assert_eq!(middle_content_left(false, 2.0), -1.0, "越界钳住");
    assert_eq!(detail_exit_left(320.0, 0.0), 0.0);
    assert_eq!(detail_exit_left(320.0, 0.5), 160.0);
    assert_eq!(detail_exit_left(320.0, 1.5), 320.0);
}

/// 中栏半路掉头不跳:收到一半又展开(或反过来),新一场从 `1 − 进度` 起步,
/// 内容层此刻的位置与掉头前一模一样(见 [`MiddleMotion`])。
#[test]
fn 中栏开合半路掉头从当前位置接着走() {
    for entering in [true, false] {
        for i in 0..=20 {
            let p = i as f32 / 20.0;
            let before = middle_content_left(entering, p);
            let after = middle_content_left(!entering, motion_kit::reversed_progress(p));
            assert!((before - after).abs() < 1e-6, "entering={entering} p={p}");
        }
    }
    // 起步进度由 `resume_band` 接上:建出来那一刻就在起步值上
    let now = std::time::Instant::now();
    let band = motion_kit::resume_band(0.6, mt_ui::motion::PANEL_SWAP, now);
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

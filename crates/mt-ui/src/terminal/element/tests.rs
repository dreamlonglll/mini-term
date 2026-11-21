use super::*;

/// ⌥+点击定位光标:同一行按列差发左右键,零位移不发。
#[test]
fn 点击定位光标合成方向键() {
    let m = TermMode::empty();
    // 同行右移 3 格
    assert_eq!(
        cursor_move_bytes(0, 2, 0, 5, m),
        b"\x1b[C\x1b[C\x1b[C".to_vec()
    );
    // 同行左移 2 格
    assert_eq!(cursor_move_bytes(0, 5, 0, 3, m), b"\x1b[D\x1b[D".to_vec());
    // 点在光标本身:什么都不发,别让一次点击白白喂 PTY 一个空包
    assert!(cursor_move_bytes(3, 7, 3, 7, m).is_empty());
}

/// **跨行一律不动**。竖向位移在行编辑器里往往是召回历史而不是移动光标 ——
/// pwsh 里点上一行,一个 Up 就把正在编辑的整行换成历史条目(上游评审实测)。
/// 这条钉住「宁可不动也不能毁掉用户正在输入的内容」。
#[test]
fn 点击定位光标跨行不动() {
    let m = TermMode::empty();
    assert!(cursor_move_bytes(1, 4, 2, 5, m).is_empty(), "往下跨行");
    assert!(cursor_move_bytes(2, 4, 1, 3, m).is_empty(), "往上跨行");
    // 跨行且同列同样不动
    assert!(cursor_move_bytes(1, 4, 5, 4, m).is_empty(), "跨行同列");
}

/// DECCKM(`APP_CURSOR`)下方向键换 SS3 前缀 —— 与 `keystroke_to_bytes` 同一口径,
/// 两处若分叉,vim 这类开了应用光标键的程序会收到它不认的序列。
#[test]
fn 点击定位光标跟随应用光标键模式() {
    assert_eq!(
        cursor_move_bytes(0, 0, 0, 2, TermMode::APP_CURSOR),
        b"\x1bOC\x1bOC".to_vec()
    );
}

/// 点到几千列开外时整个不动:走一半的结果既不是用户要的位置,又没法撤销。
#[test]
fn 点击定位光标超出步数上限即放弃() {
    let far = MAX_CURSOR_MOVE_STEPS as i32 + 1;
    assert!(cursor_move_bytes(0, 0, 0, far, TermMode::empty()).is_empty());
    // 上限本身要放行
    let at_limit = cursor_move_bytes(0, 0, 0, MAX_CURSOR_MOVE_STEPS as i32, TermMode::empty());
    assert_eq!(at_limit.len(), MAX_CURSOR_MOVE_STEPS * 3);
}

/// 图标格借不借右边那格:紧跟一个干净的空格才借,powerline 分隔符永远不借。
#[test]
fn 图标后面紧跟空格才放宽到两列() {
    let (fg, bg) = (gpui::white(), gpui::black());
    let folder = CellSignature::plain(3, '\u{f07b}', fg, bg);
    let blank = CellSignature::plain(4, ' ', fg, bg);
    assert_eq!(symbol_columns(&folder, Some(&blank)), 2, "图标 + 空格");
    assert_eq!(
        symbol_columns(&folder, Some(&CellSignature::plain(4, 'x', fg, bg))),
        1,
        "后面是字:不许压上去"
    );
    assert_eq!(symbol_columns(&folder, None), 1, "行尾");
    assert_eq!(
        symbol_columns(&folder, Some(&CellSignature::plain(5, ' ', fg, bg))),
        1,
        "下一格不是紧挨着的那列"
    );
    let mut combining = blank;
    combining.zerowidth[0] = '\u{0301}';
    assert_eq!(
        symbol_columns(&folder, Some(&combining)),
        1,
        "空格上带组合符号:有笔画"
    );
    // powerline 半圆端帽:要跟隔壁段严丝合缝,后面是空格也只占一列
    let cap = CellSignature::plain(3, '\u{e0b4}', fg, bg);
    assert_eq!(symbol_columns(&cap, Some(&blank)), 1);
}

/// 借格要两格「底下长得一样」—— 两条都是 E2E 实拍出来的:反显图标后跟普通
/// 空格,右半截白字落在白底上;块光标压在图标上,光标块被放大的图标挖空。
#[test]
fn 图标借格要两格底色光标选区一致() {
    let (fg, bg) = (gpui::white(), gpui::black());
    let folder = CellSignature::plain(3, '\u{f07b}', fg, bg);
    let blank = CellSignature::plain(4, ' ', fg, bg);

    // 反显(或带色底)的图标 + 默认底空格
    let mut inverse = folder;
    inverse.bg_default = false;
    inverse.bg = gpui::white();
    assert_eq!(symbol_columns(&inverse, Some(&blank)), 1, "反显图标");
    // 同一种色底连成一段(powerline 段里的「图标 + 空格」)照样借
    let mut segment_blank = blank;
    segment_blank.bg_default = false;
    segment_blank.bg = gpui::white();
    assert_eq!(
        symbol_columns(&inverse, Some(&segment_blank)),
        2,
        "同色底段"
    );
    let mut other_color = segment_blank;
    other_color.bg = gpui::red();
    assert_eq!(symbol_columns(&inverse, Some(&other_color)), 1, "底色不同");

    // 光标在图标上 / 在右边那个空格上
    let mut on_icon = folder;
    on_icon.cursor = 1;
    assert_eq!(symbol_columns(&on_icon, Some(&blank)), 1, "光标在图标上");
    let mut cursor_next = blank;
    cursor_next.cursor = 1;
    assert_eq!(
        symbol_columns(&folder, Some(&cursor_next)),
        1,
        "光标在空格上"
    );

    // 选区 / 查找高亮只盖了一格
    let mut selected = blank;
    selected.selected = true;
    assert_eq!(symbol_columns(&folder, Some(&selected)), 1, "选区边界");
    let mut hit = blank;
    hit.search = 1;
    assert_eq!(symbol_columns(&folder, Some(&hit)), 1, "查找命中边界");
}

/// 闪烁行的 grid 绝对行号 → 屏幕行:`row = line + display_offset`。
/// 视口顶在最新内容时(offset = 0),正数行号就是屏幕行本身。
#[test]
fn 闪烁行按显示偏移换算屏幕行() {
    assert_eq!(flash_row(0, 0, 24), Some(0));
    assert_eq!(flash_row(23, 0, 24), Some(23));
    // 回看缓冲里的行(负号)只有在往回滚够了之后才落进视口
    assert_eq!(flash_row(-5, 0, 24), None);
    assert_eq!(flash_row(-5, 5, 24), Some(0));
    assert_eq!(flash_row(-5, 10, 24), Some(5));
}

/// 落在视口外一律不画 —— 用户跳完又自己滚开了,不该在别的行上留一道底色。
#[test]
fn 闪烁行滚出视口就不画() {
    assert_eq!(flash_row(24, 0, 24), None, "越过最后一行");
    assert_eq!(flash_row(-1, 0, 24), None, "在视口上方");
    // 往回滚太多:那一行被顶到视口下面去了
    assert_eq!(flash_row(0, 24, 24), None);
    assert_eq!(flash_row(0, 23, 24), Some(23));
}

/// 光标格 = 元素原点 + 行列步进。IME 的候选框贴的就是这个矩形。
#[test]
fn 光标格按行列换算元素坐标() {
    let origin = point(px(100.0), px(50.0));
    let cell = size(px(8.0), px(16.0));
    // 左上角那一格
    let b = cursor_cell_bounds(origin, cell, 80, 24, 0, 0, 0);
    assert_eq!(b.origin, origin);
    assert_eq!(b.size, cell);
    // 第 3 行第 5 列
    let b = cursor_cell_bounds(origin, cell, 80, 24, 3, 5, 0);
    assert_eq!(b.origin, point(px(140.0), px(98.0)));
    // 回看缓冲里的负行号:滚够了才落回视口(与 flash_row 同一口径)
    let b = cursor_cell_bounds(origin, cell, 80, 24, -5, 0, 5);
    assert_eq!(b.origin.y, px(50.0));
}

/// **本 bug 的回归**:Ink 系 TUI(Claude Code)发 `ESC[?25l` 藏掉光标后,
/// 没有任何 cell 带光标标记 —— 但光标格照样要算得出来,否则 IME 候选框
/// 和预编辑串双双退回元素左上角,中文候选窗糊在终端顶行上。
///
/// 这个函数**根本不看 `CursorShape`**,可见性进不来就是这条保证。
#[test]
fn 光标藏起来时照样给得出格子() {
    let origin = point(px(0.0), px(0.0));
    let cell = size(px(10.0), px(20.0));
    // 光标在第 12 行第 30 列,shape 是 Hidden 与否都不影响这里
    let b = cursor_cell_bounds(origin, cell, 80, 24, 12, 30, 0);
    assert_eq!(b.origin, point(px(300.0), px(240.0)));
    assert_ne!(b.origin, origin, "绝不能退回元素左上角");
}

fn ime_frame(composing: bool, cursor_x: f32) -> FrameGeometry {
    FrameGeometry {
        cursor: Some(Bounds::new(
            point(px(cursor_x), px(0.0)),
            size(px(10.0), px(20.0)),
        )),
        composing,
        ..FrameGeometry::default()
    }
}

/// **issue #88 的回归**:gpui 一轮组合只定位一次,这里是唯一的纠错机会。
/// 第一帧哪怕坐标没变也要报,否则输入法抢先取到的旧坐标就纠正不回来。
#[test]
fn 组合后第一帧无条件同步候选窗坐标() {
    let idle = ime_frame(false, 100.0);
    assert!(ime_anchor_needs_sync(&idle, &ime_frame(true, 100.0)));
    // 从没画过一帧就进了组合
    assert!(ime_anchor_needs_sync(
        &FrameGeometry::default(),
        &ime_frame(true, 100.0)
    ));
}

/// 组合中只有锚点真挪了才同步:每帧都报等于每帧都让输入法重排候选窗。
#[test]
fn 组合中只在锚点挪动时同步() {
    let composing = ime_frame(true, 100.0);
    assert!(!ime_anchor_needs_sync(&composing, &ime_frame(true, 100.0)));
    assert!(ime_anchor_needs_sync(&composing, &ime_frame(true, 110.0)));
}

/// 没在组合时候选窗不存在,光标怎么动都不碰平台。
#[test]
fn 没在组合时不同步() {
    assert!(!ime_anchor_needs_sync(
        &ime_frame(false, 100.0),
        &ime_frame(false, 110.0)
    ));
    assert!(!ime_anchor_needs_sync(
        &ime_frame(true, 100.0),
        &ime_frame(false, 110.0)
    ));
}

/// IME 现读光标时用上一帧的网格换算,回看位置也要算进去。
#[test]
fn 现读的光标按帧几何换算成屏幕矩形() {
    let geometry = FrameGeometry {
        origin: point(px(100.0), px(50.0)),
        cell_size: size(px(8.0), px(16.0)),
        columns: 80,
        screen_lines: 24,
        display_offset: 2,
        ..FrameGeometry::default()
    };
    let bounds = geometry.cell_of(SettledCursor {
        line: 3,
        column: 5,
        pending: false,
    });
    assert_eq!(bounds.origin, point(px(140.0), px(130.0)));
    assert_eq!(bounds.size, geometry.cell_size);
}

/// 连字可以换字形,但**换完总共还得占 N 列**。守恒才放行,不守恒就退回禁连字
/// 重 shape —— 否则这一段里连字之后的每个字符都会整体左移。
#[test]
fn 连字总宽守恒才放行() {
    let cell = px(8.0);
    // `=>` 换成一个两列宽的连字:总宽不变
    assert!(width_fits_columns(px(16.0), cell, 2));
    // 浮点累加的余量以内照样算守恒
    assert!(width_fits_columns(px(16.4), cell, 2));
    assert!(width_fits_columns(px(15.6), cell, 2));
    // 两个字符缩成一列宽:段内后面的字全要左移一列
    assert!(!width_fits_columns(px(8.0), cell, 2));
    assert!(!width_fits_columns(px(16.6), cell, 2));
    // 长段里塌掉一列也判负(80 列的整行是常态)
    assert!(width_fits_columns(px(640.0), cell, 80));
    assert!(!width_fits_columns(px(632.0), cell, 80));
}

/// 滚出视口的光标钳到最近边缘,而不是没有 —— 往回翻历史时光标在视口下方,
/// 这时候开始打字,候选框贴在底边比贴在顶角合理。
#[test]
fn 光标滚出视口钳到最近边缘() {
    let origin = point(px(0.0), px(0.0));
    let cell = size(px(10.0), px(20.0));
    // 往回滚 100 行:光标被顶到视口下面 → 钳到最后一行
    let b = cursor_cell_bounds(origin, cell, 80, 24, 0, 0, 100);
    assert_eq!(b.origin.y, px(460.0), "第 23 行 = (24-1) * 20");
    // 列越界钳到最后一列(宽字符占位等边界情形)
    let b = cursor_cell_bounds(origin, cell, 80, 24, 0, 999, 0);
    assert_eq!(b.origin.x, px(790.0), "第 79 列 = (80-1) * 10");
    // 退化尺寸不 panic、不算出负坐标
    let b = cursor_cell_bounds(origin, cell, 0, 0, 0, 0, 0);
    assert_eq!(b.origin, origin);
}

#[test]
fn 布局动画期_grid_resize_冻结闸状态切换() {
    assert!(!is_grid_resize_frozen());
    assert!(!set_grid_resize_frozen(false));
    assert!(set_grid_resize_frozen(true));
    assert!(is_grid_resize_frozen());
    assert!(!set_grid_resize_frozen(true));
    assert!(set_grid_resize_frozen(false));
    assert!(!is_grid_resize_frozen());
}

use super::*;

/// 从第 `line` 行选到第 `end` 行。
fn range(line: u32, end: u32) -> LineTarget {
    LineTarget {
        end: Some(end),
        ..LineTarget::line(line)
    }
}

#[test]
fn 范围夹进文件行数() {
    let total = 100;
    assert_eq!(
        LineTarget::line(20).clamp(total),
        Some(LineTarget::line(20))
    );
    assert_eq!(
        range(1, 222).clamp(total),
        Some(range(1, 100)),
        "终点越界夹到末行"
    );
    assert_eq!(
        range(30, 30).clamp(total),
        Some(range(30, 30)),
        "起止同一行 = 选中这一整行"
    );
    assert_eq!(
        range(30, 20).clamp(total),
        Some(LineTarget::line(30)),
        "终点比起点小当没写"
    );
    assert_eq!(LineTarget::line(101).clamp(total), None, "起点越界不定位");
    assert_eq!(LineTarget::line(0).clamp(total), None, "行号是 1-based");
    // 滚到正中的标记夹过之后还在
    let centered = LineTarget::line(20).centered();
    assert!(centered.center && !LineTarget::line(20).center);
    assert_eq!(centered.clamp(total), Some(centered));
}

#[test]
fn 目标行顶到视口顶() {
    let lh = px(20.);
    let viewport = px(200.);
    // 文件中段:行顶正好就是滚动量
    assert_eq!(top_scroll(px(400.), lh, 80, lh, viewport), px(400.));
    // 文件头:不滚
    assert_eq!(top_scroll(px(0.), lh, 99, lh, viewport), px(0.));
}

#[test]
fn 文件尾那一屏夹到最大滚动量() {
    let lh = px(20.);
    let viewport = px(200.);
    // 100 行的文件内容高 2000,最多滚 1800;第 96 行(行顶 1900)顶不到最上面
    assert_eq!(top_scroll(px(1900.), lh, 4, lh, viewport), px(1800.));
    // 末行
    assert_eq!(top_scroll(px(1980.), lh, 0, lh, viewport), px(1800.));
    // 整份文件不到一屏:不滚
    assert_eq!(top_scroll(px(60.), lh, 2, lh, viewport), px(0.));
}

#[test]
fn 软换行的长行以整行高度算() {
    let lh = px(20.);
    let viewport = px(200.);
    // 目标行折成 3 个视觉行(高 60),下面还有 1 行:内容高下界 = 1000 + 60 + 20
    let y = top_scroll(px(1000.), px(60.), 1, lh, viewport);
    assert_eq!(y, px(880.));
    // 夹过之后目标行仍整行在视口里
    assert!(px(1000.) - y + px(60.) <= viewport);
}

/// 快速打开的跳行:目标行落在视口正中(上下各留半屏上下文);文件头尾那一屏夹在滚动范围里;
/// 比视口还高的行退回顶到视口顶上。
#[test]
fn 目标行滚到视口正中() {
    let lh = px(20.);
    let viewport = px(200.);
    // 文件中段(100 行,第 41 行行顶 800):行的正中对着视口正中
    let y = center_scroll(px(800.), lh, 59, lh, viewport);
    assert_eq!(y, px(710.));
    assert_eq!(px(800.) - y + lh / 2., viewport / 2.);
    // 文件头:不滚过首行
    assert_eq!(center_scroll(px(40.), lh, 97, lh, viewport), px(0.));
    // 文件尾:夹到最大滚动量 1800
    assert_eq!(center_scroll(px(1960.), lh, 1, lh, viewport), px(1800.));
    // 整份文件不到一屏:不滚
    assert_eq!(center_scroll(px(60.), lh, 2, lh, viewport), px(0.));
    // 软换行的长段比视口还高:顶到视口顶上,保住它的开头
    assert_eq!(
        center_scroll(px(1000.), px(300.), 20, lh, viewport),
        px(1000.)
    );
}

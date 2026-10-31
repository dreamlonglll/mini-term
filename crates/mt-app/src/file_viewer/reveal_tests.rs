use super::*;

#[test]
fn 范围夹进文件行数() {
    let total = 100;
    assert_eq!(
        LineTarget::line(20).clamp(total),
        Some(LineTarget::line(20))
    );
    assert_eq!(
        LineTarget {
            line: 1,
            end: Some(222)
        }
        .clamp(total),
        Some(LineTarget {
            line: 1,
            end: Some(100)
        }),
        "终点越界夹到末行"
    );
    assert_eq!(
        LineTarget {
            line: 30,
            end: Some(30)
        }
        .clamp(total),
        Some(LineTarget {
            line: 30,
            end: Some(30)
        }),
        "起止同一行 = 选中这一整行"
    );
    assert_eq!(
        LineTarget {
            line: 30,
            end: Some(20)
        }
        .clamp(total),
        Some(LineTarget::line(30)),
        "终点比起点小当没写"
    );
    assert_eq!(LineTarget::line(101).clamp(total), None, "起点越界不定位");
    assert_eq!(LineTarget::line(0).clamp(total), None, "行号是 1-based");
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

//! 端到端(不起 PTY):字节进 [`TerminalEmulator`],从 [`Kitty::visible_images`] 读结果。

use alacritty_terminal::event::Event;
use alacritty_terminal::index::{Column, Line};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::decode::encode_png;
use super::*;
use crate::width::WidthOverride;
use crate::{TermSize, TerminalEmulator};

const BSU: &[u8] = b"\x1b[?2026h";
const ESU: &[u8] = b"\x1b[?2026l";

/// 开着图形、一格 10×20 像素的终端。
fn terminal(columns: usize, lines: usize) -> TerminalEmulator {
    let e = TerminalEmulator::with_scrollback(TermSize::new(columns, lines), 100)
        .with_kitty_graphics(true);
    e.kitty().expect("开着").set_cell_pixels(10.0, 20.0);
    e
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![200u8; (width * height * 4) as usize];
    encode_png(width, height, &pixels)
}

/// 一条完整的(不分块的)传输命令。
fn apc(control: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("\x1b_G{control};").into_bytes();
    out.extend_from_slice(STANDARD.encode(payload).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// pi 的分块形态:第一块带全部控制键 + `m=1`,中间 `m=1`,最后 `m=0`。
fn chunked(control: &str, payload: &[u8], chunk: usize) -> Vec<u8> {
    let encoded = STANDARD.encode(payload);
    let bytes = encoded.as_bytes();
    let mut out = Vec::new();
    let pieces: Vec<&[u8]> = bytes.chunks(chunk).collect();
    for (i, piece) in pieces.iter().enumerate() {
        let last = i + 1 == pieces.len();
        let head = match (i == 0, last) {
            (true, true) => format!("\x1b_G{control};"),
            (true, false) => format!("\x1b_G{control},m=1;"),
            (false, false) => "\x1b_Gm=1;".to_string(),
            (false, true) => "\x1b_Gm=0;".to_string(),
        };
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(piece);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

fn control(text: &str) -> Vec<u8> {
    format!("\x1b_G{text}\x1b\\").into_bytes()
}

fn images(e: &TerminalEmulator) -> Vec<VisibleImage> {
    e.with_term(|term| e.kitty().expect("开着").visible_images(term))
}

/// `(行, 列, 列数, 行数)`。
fn layout(e: &TerminalEmulator) -> Vec<(i32, usize, u32, u32)> {
    images(e)
        .iter()
        .map(|image| (image.line, image.column, image.columns, image.rows))
        .collect()
}

fn cursor(e: &TerminalEmulator) -> (i32, usize) {
    e.with_term(|term| {
        let point = term.grid().cursor.point;
        (point.line.0, point.column.0)
    })
}

fn store_counts(e: &TerminalEmulator) -> (usize, usize) {
    let store = e.kitty().expect("开着").shared.store.lock();
    (store.image_count(), store.placement_count())
}

fn replies(e: &TerminalEmulator) -> Vec<String> {
    e.events()
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            Event::PtyWrite(text) => Some(text),
            _ => None,
        })
        .collect()
}

// ───────────────────────── 开关 ─────────────────────────

/// **默认关闭**:字节原样进 vte(它把 APC 吞掉),OSC 8 原样转发,什么都不回。
#[test]
fn 默认关闭时不介入() {
    let e = TerminalEmulator::new(TermSize::new(40, 6));
    assert!(e.kitty().is_none());
    e.advance(&apc("a=T,f=100,i=1,c=4,r=2", &png(4, 4)));
    e.advance(b"text\x1b[16t");
    assert_eq!(e.visible_lines()[0], "text");
    assert!(replies(&e).is_empty(), "CSI 16 t 没开时不答");

    // 关着的时候,私有 scheme 就是一条普通的 OSC 8
    e.advance(b"\r\n\x1b]8;;mt-kitty:00:1\x1b\\link\x1b]8;;\x1b\\");
    e.with_term(|term| {
        let link = term.grid()[Line(1)][Column(0)].hyperlink().expect("转发了");
        assert_eq!(link.uri(), "mt-kitty:00:1");
    });
}

/// `with_kitty_graphics(false)` 就是没开。
#[test]
fn 开关传假时等于没开() {
    let e = TerminalEmulator::new(TermSize::new(10, 2)).with_kitty_graphics(false);
    assert!(e.kitty().is_none());
}

// ───────────────────────── 传输与摆放 ─────────────────────────

#[test]
fn 传输并摆放() {
    let e = terminal(40, 10);
    e.advance(b"abc\r\n");
    e.advance(&apc("a=T,f=100,q=2,C=1,c=4,r=2,i=7", &png(8, 8)));
    assert_eq!(layout(&e), vec![(1, 0, 4, 2)]);
    assert_eq!(cursor(&e), (1, 0), "不移动光标");
    assert_eq!(e.visible_lines()[0], "abc");
    assert_eq!(e.visible_lines()[1], "", "摆放点的文字不变");
    assert_eq!(store_counts(&e), (1, 1));
}

/// pi 那一帧:`\r\n\n\n\n\n` 预留行 → 上移 → 顶行发图 → 下移。
#[test]
fn pi_的一帧落点正确() {
    let e = terminal(80, 24);
    let mut frame = BSU.to_vec();
    frame.extend_from_slice(b"\r\n\n\n\n\n\x1b[5A");
    frame.extend_from_slice(&chunked(
        "a=T,f=100,q=2,C=1,c=40,r=6,i=3525535711",
        &png(100, 100),
        16,
    ));
    frame.extend_from_slice(b"\x1b[5B");
    frame.extend_from_slice(ESU);
    e.advance(&frame);
    assert_eq!(layout(&e), vec![(0, 0, 40, 6)], "预留行的第一行");
    assert_eq!(images(&e)[0].source.width, 100);
    assert_eq!(cursor(&e), (5, 0));
    assert!(e.visible_lines().iter().all(String::is_empty), "没有漏字");
}

/// PTY 把序列切到任意位置,结果都一样。
#[test]
fn 任意切分结果都一样() {
    let mut stream = b"head\r\n".to_vec();
    stream.extend_from_slice(BSU);
    stream.extend_from_slice(b"\x1b[3;2H");
    stream.extend_from_slice(&chunked("a=T,f=100,q=2,c=4,r=2,i=9", &png(4, 4), 12));
    stream.extend_from_slice(ESU);
    stream.extend_from_slice(b"\x1b[6;1H\x1b_junk\x1b\\tail");
    for size in [1, 2, 3, 5, 7, 11, 64, 4096] {
        let e = terminal(40, 10);
        for chunk in stream.chunks(size) {
            e.advance(chunk);
        }
        assert_eq!(layout(&e), vec![(2, 1, 4, 2)], "切成 {size} 字节一块");
        assert_eq!(e.visible_lines()[0], "head");
        assert_eq!(
            e.visible_lines()[5],
            "tail",
            "切成 {size} 字节一块:别的 APC 不漏字"
        );
    }
}

/// 只传不摆:留着数据,没有摆放。
#[test]
fn 只传不摆() {
    let e = terminal(40, 10);
    e.advance(&apc("a=t,f=100,q=2,i=5", &png(4, 4)));
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (1, 0));
    // 缺省动作就是 a=t
    e.advance(&apc("f=100,q=2,i=6", &png(4, 4)));
    assert_eq!(store_counts(&e), (2, 0));
    assert!(layout(&e).is_empty());
}

/// **全屏模式**(pi 默认)的主路:传一次,之后 `d=a` + `a=p`。
#[test]
fn 全屏模式的重放() {
    let e = terminal(40, 10);
    e.advance(b"\x1b[?1049h");
    let mut first = BSU.to_vec();
    first.extend_from_slice(&control("a=d,d=A,q=2"));
    first.extend_from_slice(b"\x1b[2J\x1b[2;1H\x1b[2K");
    first.extend_from_slice(&apc("a=T,f=100,q=2,C=1,c=4,r=2,i=7", &png(8, 8)));
    first.extend_from_slice(ESU);
    e.advance(&first);
    assert_eq!(layout(&e), vec![(1, 0, 4, 2)]);
    let uid = images(&e)[0].bitmap.uid();

    let mut scrolled = BSU.to_vec();
    scrolled.extend_from_slice(&control("a=d,d=a,q=2"));
    scrolled.extend_from_slice(b"\x1b[2;1H\x1b[2K\x1b[5;1H\x1b[2K");
    scrolled.extend_from_slice(&control("a=p,q=2,i=7,c=4,r=2,C=1"));
    scrolled.extend_from_slice(ESU);
    e.advance(&scrolled);
    assert_eq!(layout(&e), vec![(4, 0, 4, 2)]);
    assert_eq!(images(&e)[0].bitmap.uid(), uid, "不重传,还是那张位图");
    assert_eq!(store_counts(&e), (1, 1), "旧摆放撤了");

    // 滚到一半:裁切上半截,只摆 1 行
    let mut cropped = BSU.to_vec();
    cropped.extend_from_slice(&control("a=d,d=a,q=2"));
    cropped.extend_from_slice(b"\x1b[5;1H\x1b[2K\x1b[1;1H\x1b[2K");
    cropped.extend_from_slice(&control("a=p,q=2,i=7,c=4,C=1,y=4,h=4,r=1"));
    cropped.extend_from_slice(ESU);
    e.advance(&cropped);
    assert_eq!(layout(&e), vec![(0, 0, 4, 1)]);
    assert_eq!(
        images(&e)[0].source,
        SourceRect {
            x: 0,
            y: 4,
            width: 8,
            height: 4
        }
    );
}

/// 虚拟摆放(Unicode 占位符用的)与相对摆放:图留着,不摆。
#[test]
fn 虚拟与相对摆放不画() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,U=1,c=4,r=2,i=1", &png(4, 4)));
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (1, 0), "图留着");

    e.advance(&control("a=p,q=2,U=1,i=1,c=4,r=2"));
    e.advance(&control("a=p,q=2,P=1,Q=2,i=1,c=4,r=2"));
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (1, 0));
}

/// 摆一张没传过的图:什么都不发生;应答 ENOENT。
#[test]
fn 摆放不存在的图() {
    let e = terminal(40, 10);
    e.advance(&control("a=p,i=404,c=4,r=2"));
    assert!(layout(&e).is_empty());
    assert_eq!(replies(&e), ["\x1b_Gi=404;ENOENT:image not found\x1b\\"]);
}

/// 裁切矩形越界:收回图内;整个在图外不摆。
#[test]
fn 裁切矩形收进原图() {
    let e = terminal(40, 10);
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(10, 10)));
    e.advance(&control("a=p,q=2,i=1,x=6,y=2,w=100,h=100,c=2,r=2"));
    assert_eq!(
        images(&e)[0].source,
        SourceRect {
            x: 6,
            y: 2,
            width: 4,
            height: 8
        }
    );
    e.advance(b"\x1b[2J");
    e.advance(&control("a=p,i=1,x=10,c=2,r=2"));
    assert!(layout(&e).is_empty());
    assert_eq!(
        replies(&e),
        ["\x1b_Gi=1;EINVAL:source rectangle out of bounds\x1b\\"]
    );
}

/// 范围之外的别家协议:iTerm2 的 OSC 1337 与 Sixel 的 DCS 不显示乱码,也不进账本。
#[test]
fn 别家图片协议不显示乱码() {
    let e = terminal(40, 6);
    let payload = STANDARD.encode(png(4, 4));
    e.advance(format!("\x1b]1337;File=inline=1;size=9:{payload}\x07").as_bytes());
    e.advance(b"\x1bPq#0;2;0;0;0#1;2;100;100;0~~~~-~~~~\x1b\\");
    e.advance(b"ok");
    assert_eq!(e.visible_lines()[0], "ok");
    assert_eq!(store_counts(&e), (0, 0));
    assert!(layout(&e).is_empty());
}

// ───────────────────────── 缺省的 c、r ─────────────────────────

#[test]
fn 没给行列时按原尺寸算() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,i=1", &png(25, 41)));
    assert_eq!(layout(&e), vec![(0, 0, 3, 3)], "25/10 → 3,41/20 → 3");
}

#[test]
fn 只给一个时另一个按比例算() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,i=1,c=4", &png(40, 80)));
    assert_eq!(
        layout(&e),
        vec![(0, 0, 4, 4)],
        "4 列 = 40px → 高 80px = 4 行"
    );
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,i=1,r=2", &png(40, 80)));
    assert_eq!(
        layout(&e),
        vec![(0, 0, 2, 2)],
        "2 行 = 40px 高 → 宽 20px = 2 列"
    );
}

#[test]
fn 行列数有上限() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,i=1,c=4000000,r=4000000", &png(4, 4)));
    assert_eq!(layout(&e), vec![(0, 0, MAX_CELLS, MAX_CELLS)]);
}

// ───────────────────────── 按显示尺寸存 ─────────────────────────

/// 一张大图摆在小格子里,只存显示尺寸。
#[test]
fn 按显示尺寸缩小() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,i=1,c=4,r=2", &png(400, 400)));
    let image = &images(&e)[0];
    assert_eq!(image.bitmap.size(), (40, 40), "4×10 / 2×20");
    assert_eq!(image.bitmap.source_size(), (400, 400));
    assert_eq!(image.source.width, 400, "裁切矩形仍按原图算");
    assert_eq!(
        e.kitty().unwrap().shared.store.lock().bytes(),
        40 * 40 * 4,
        "账本按缩完的算"
    );
}

/// 显示得比原图大:不放大。
#[test]
fn 不比原图大() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,i=1,c=20,r=8", &png(16, 16)));
    assert_eq!(images(&e)[0].bitmap.size(), (16, 16));
}

/// 第一次传就带裁切(滚到一半才发的图):按裁切部分铺满来算整张图的比例。
#[test]
fn 带裁切的传输按裁切部分算比例() {
    let e = terminal(40, 10);
    // 400×400 只露下半截 200 行,摆成 4 列 × 1 行(40×20 px)
    e.advance(&apc(
        "a=T,f=100,q=2,i=1,c=4,r=1,y=200,h=200",
        &png(400, 400),
    ));
    let image = &images(&e)[0];
    assert_eq!(
        image.bitmap.size(),
        (40, 40),
        "露出的部分 200 行 → 20px,整张 40px"
    );
    assert_eq!(image.source.y, 200);
}

/// `a=t` 不知道会显示多大,没超上限就不缩。
#[test]
fn 只传不摆时不缩() {
    let e = terminal(40, 10);
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(400, 400)));
    e.advance(&control("a=p,q=2,i=1,c=4,r=2"));
    assert_eq!(images(&e)[0].bitmap.size(), (400, 400));
}

/// 超上限的按长边等比封顶。裁切坐标按原图算,所以 `source_size` 不变。
#[test]
fn 只传不摆的大图按长边封顶() {
    assert_eq!(
        target_bitmap_size((4096, 1024), None, None),
        (MAX_UNPLACED_SIDE, 512)
    );
    assert_eq!(
        target_bitmap_size((1024, 4096), None, None),
        (512, MAX_UNPLACED_SIDE)
    );
    assert_eq!(target_bitmap_size((2048, 16), None, None), (2048, 16));

    let e = terminal(40, 10);
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(4096, 1024)));
    e.advance(&control("a=p,q=2,i=1,c=4,r=2"));
    let image = images(&e).remove(0);
    assert_eq!(image.bitmap.size(), (2048, 512));
    assert_eq!(image.bitmap.source_size(), (4096, 1024));
}

/// 渲染层还没报过一格多大(后台 pane 从没画过):不缩,摆放按兜底格算。
#[test]
fn 没报过格子大小时不缩() {
    let e = TerminalEmulator::with_scrollback(TermSize::new(40, 10), 100).with_kitty_graphics(true);
    e.advance(&apc("a=T,f=100,q=2,i=1,c=4,r=2", &png(400, 400)));
    assert_eq!(images(&e)[0].bitmap.size(), (400, 400));
    assert_eq!(e.kitty().unwrap().reported_cell_pixels(), (10, 20));
}

// ───────────────────────── 顺序 ─────────────────────────

/// **同步块里的命令要等块结束才生效**,落点是字节流里它那个位置的光标 ——
/// 这是带内标记存在的全部理由。预解析器看到命令时光标还在第 0 行。
#[test]
fn 同步块里按字节流顺序落位() {
    let e = terminal(40, 10);
    e.advance(BSU);
    e.advance(b"\x1b[4;3H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.advance(b"\x1b[8;8H");
    assert!(layout(&e).is_empty(), "同步块没结束,还没生效");
    assert_eq!(store_counts(&e), (0, 0), "也没入库");
    assert_eq!(cursor(&e), (0, 0), "grid 还没动");
    e.advance(ESU);
    assert_eq!(layout(&e), vec![(3, 2, 2, 2)], "落在发图时的光标上");
    assert_eq!(cursor(&e), (7, 7));
}

/// 同步块之外的命令当场生效。
#[test]
fn 同步块之外当场生效() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    assert_eq!(layout(&e).len(), 1);
}

/// **一帧里的删除 + 重发是原子的**:同步块结束之前旧图一直画着,结束那一刻换成新图,
/// 中间没有空窗。pi 普通模式每次重绘都走这条路。
#[test]
fn 删除加重发不闪空() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    let old = images(&e)[0].bitmap.uid();

    e.advance(BSU);
    e.advance(&control("a=d,d=I,i=1,q=2"));
    assert_eq!(images(&e)[0].bitmap.uid(), old, "还没到块尾,旧图不能先没");
    e.advance(b"\x1b[2K");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    assert_eq!(images(&e)[0].bitmap.uid(), old);
    e.advance(ESU);

    let now = images(&e);
    assert_eq!(now.len(), 1);
    assert_ne!(now[0].bitmap.uid(), old);
    assert_eq!(store_counts(&e), (1, 1), "旧图放掉了,不累积");
}

/// 图片 payload 不进 vte 的同步缓冲:3 MiB 的图夹在同步块里,vte 不会因缓冲满而提前放行。
#[test]
fn 大图不占同步缓冲() {
    // 噪声不好压,PNG 体积接近原始大小
    let side = 1024u32;
    let mut seed = 0x1234_5678u32;
    let pixels: Vec<u8> = (0..side * side * 4)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let big = encode_png(side, side, &pixels);
    assert!(big.len() > 3 * 1024 * 1024, "{}", big.len());

    let e = terminal(40, 10);
    e.advance(BSU);
    e.advance(b"before");
    e.advance(&chunked("a=T,f=100,q=2,c=2,r=2,i=1", &big, 4096));
    assert_eq!(e.visible_lines()[0], "", "还在同步块里,字不该提前上屏");
    e.advance(b"\x1b[3;1H-after");
    e.advance(ESU);
    assert_eq!(e.visible_lines()[0], "before");
    assert_eq!(e.visible_lines()[2], "-after");
    assert_eq!(layout(&e), vec![(0, 6, 2, 2)]);
}

/// 同步块超时释放(`vte::Processor::stop_sync`,并行分支 `release_expired_sync` 走的就是
/// 这条路):**分块传到一半放行**。放出来的字节照常上屏,剩下的分块继续传,收尾时落在
/// 放行之后的光标上;标记在释放路径上同样被拦。
#[test]
fn 同步块超时释放时图形状态接得上() {
    let e = terminal(40, 10);
    let whole = chunked("a=T,f=100,q=2,c=2,r=2,i=1", &png(32, 32), 16);
    let (head, tail) = whole.split_at(whole.len() / 2);
    assert!(
        head.windows(2).any(|w| w == b"\x1b\\"),
        "切在分块之间才有意义"
    );

    e.advance(BSU);
    e.advance(b"frozen");
    e.advance(head);
    assert_eq!(e.visible_lines()[0], "");

    let force_release = |e: &TerminalEmulator| {
        let mut term = e.term.lock();
        let mut parser = e.parser.lock();
        let parser = &mut *parser;
        parser
            .vte
            .stop_sync(&mut WidthOverride::new(&mut term, &mut parser.cluster));
    };
    force_release(&e);
    assert_eq!(e.visible_lines()[0], "frozen");
    assert!(layout(&e).is_empty(), "还没传完");

    e.advance(tail);
    e.advance(ESU);
    assert_eq!(layout(&e), vec![(0, 6, 2, 2)]);

    // 整条命令攒在块里、释放时才走到标记:释放路径同样要执行它,不能当普通链接转发
    let e = terminal(40, 10);
    e.advance(BSU);
    e.advance(b"\x1b[3;1H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    assert!(layout(&e).is_empty());
    force_release(&e);
    assert_eq!(layout(&e), vec![(2, 0, 2, 2)]);
    e.with_term(|term| {
        let template = &term.grid().cursor.template;
        assert!(template.hyperlink().is_none(), "标记不能挂到光标模板上");
    });
}

// ───────────────────────── 伪造 ─────────────────────────

/// 程序自己发私有 scheme 的 OSC 8:nonce 对不上,什么都不发生,也不会给后面的字挂上链接。
#[test]
fn 伪造标记被忽略() {
    let e = terminal(40, 10);
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(4, 4)));
    let (images_before, placements_before) = store_counts(&e);

    for forged in [
        &b"\x1b]8;;mt-kitty:00000000000000000000000000000000:1\x1b\\"[..],
        b"\x1b]8;;mt-kitty:\x1b\\",
        b"\x1b]8;;mt-kitty:deadbeef:9999\x1b\\",
        b"\x1b]8;id=x;mt-kitty:deadbeef:1\x07",
    ] {
        e.advance(forged);
        e.advance(b"x");
    }
    assert_eq!(store_counts(&e), (images_before, placements_before));
    e.with_term(|term| {
        let row = &term.grid()[Line(0)];
        for column in 0..4 {
            assert!(
                row[Column(column)].hyperlink().is_none(),
                "第 {column} 列被挂上了"
            );
        }
        assert!(term.grid().cursor.template.hyperlink().is_none());
    });
    assert_eq!(e.visible_lines()[0], "xxxx");
}

/// 就算猜中了 nonce,序号不在队列里也不起作用;用过的序号不能重放。
#[test]
fn 猜中_nonce_也不起作用() {
    let e = terminal(40, 10);
    let prefix = e.kitty().unwrap().shared.prefix.clone();
    e.advance(format!("\x1b]8;;{prefix}12345\x1b\\").as_bytes());
    assert_eq!(store_counts(&e), (0, 0));

    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    assert_eq!(store_counts(&e), (1, 1));
    // 刚才那条用的序号是 1(队列里第一条)
    e.advance(b"\x1b[5;5H");
    e.advance(format!("\x1b]8;;{prefix}1\x1b\\").as_bytes());
    assert_eq!(store_counts(&e), (1, 1), "重放不生效");
    assert_eq!(layout(&e), vec![(0, 0, 2, 2)]);
    assert_eq!(e.visible_lines().join(""), "");
}

/// 真正的 OSC 8 不受影响;摆放不打断正在进行的链接。
#[test]
fn 普通链接不受影响() {
    let e = terminal(40, 10);
    e.advance(b"\x1b]8;;https://example.com\x1b\\ab");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.advance(b"cd\x1b]8;;\x1b\\ef");
    e.with_term(|term| {
        let row = &term.grid()[Line(0)];
        for column in 0..4 {
            assert_eq!(
                row[Column(column)].hyperlink().expect("链接里").uri(),
                "https://example.com",
                "第 {column} 列"
            );
        }
        assert!(row[Column(4)].hyperlink().is_none());
    });
    // 图摆在第 2 列,随后被 c 覆盖写:锚没了,这是已知限制
    assert!(layout(&e).is_empty());
}

/// 锚所在的格子是空白:不是 http(s),并行分支的链接逻辑不会点亮它;文字也不变。
#[test]
fn 锚点格子是空白且不是网址() {
    let e = terminal(40, 10);
    e.advance(b"\x1b[2;5H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.with_term(|term| {
        let cell = &term.grid()[Line(1)][Column(4)];
        assert_eq!(cell.c, ' ');
        let link = cell.hyperlink().expect("锚");
        assert!(link.uri().starts_with(SCHEME));
        assert!(!link.uri().starts_with("http"));
    });
    assert!(e.visible_lines().iter().all(String::is_empty));
    let text = e.with_term_mut(|term| {
        use alacritty_terminal::index::{Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut selection = Selection::new(
            SelectionType::Lines,
            Point::new(Line(1), Column(0)),
            Side::Left,
        );
        selection.update(Point::new(Line(1), Column(39)), Side::Right);
        term.selection = Some(selection);
        term.selection_to_string()
    });
    assert_eq!(text.unwrap_or_default().trim(), "", "选区复制不多字");
}

// ───────────────────────── 删除 ─────────────────────────

/// `d=i` 留数据、`d=I` 放数据,`a=p` 能不能再摆是最直接的判据。
#[test]
fn 按_id_删除的大小写语义() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.advance(&control("a=d,d=i,i=1,q=2"));
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (1, 0), "小写留数据");

    e.advance(&control("a=p,q=2,i=1,c=2,r=2"));
    assert_eq!(layout(&e).len(), 1, "还能再摆");

    e.advance(&control("a=d,d=I,i=1,q=2"));
    assert_eq!(store_counts(&e), (0, 0), "大写放数据");
    e.advance(&control("a=p,q=2,i=1,c=2,r=2"));
    assert!(layout(&e).is_empty(), "摆不出来了");
}

/// `d=a` / `d=A` 只管**屏幕上看得见的**;滚进回看的不动。
#[test]
fn 删全部只管屏上的() {
    let e = terminal(40, 4);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=1", &png(4, 4))); // 这张滚进回看
    e.advance(b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=2", &png(4, 4))); // 这张在屏上
    assert_eq!(store_counts(&e), (2, 2));

    e.advance(&control("a=d,d=a,q=2"));
    assert_eq!(store_counts(&e), (2, 1), "屏上那张撤了,数据留着");

    // 翻回去看:回看里那张还在
    e.with_term_mut(|term| term.scroll_display(alacritty_terminal::grid::Scroll::Top));
    assert_eq!(layout(&e).len(), 1);

    // 缺省 d 就是 a;屏上没东西了,回看的照样不动
    e.advance(&control("a=d,q=2"));
    assert_eq!(store_counts(&e).1, 1);
}

#[test]
fn 大写的删全部放掉数据且留下还被引用的() {
    let e = terminal(40, 4);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=1", &png(4, 4)));
    e.advance(b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n");
    e.advance(&control("a=p,q=2,i=1,c=2,r=1")); // 同一张图再摆到屏上
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=2", &png(4, 4))); // 覆盖同一格 → 前一个锚没了
    assert_eq!(store_counts(&e).0, 2);

    e.advance(&control("a=d,d=A,q=2"));
    let (images_left, _) = store_counts(&e);
    assert_eq!(images_left, 1, "回看里还引用着的图 1 留下,图 2 放掉");
}

/// 探到屏上的半截也算「看得见」。
#[test]
fn 从视口上方探下来的也算屏上() {
    let e = terminal(40, 4);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=3,i=1", &png(4, 4)));
    e.advance(b"\r\n\r\n\r\n\r\n\r\n");
    assert_eq!(layout(&e), vec![(-2, 0, 2, 3)], "下半截还在屏上");
    e.advance(&control("a=d,d=a,q=2"));
    assert!(layout(&e).is_empty());
}

/// 不认识的删除范围:吞掉,什么都不做。
#[test]
fn 范围外的删除被吞掉() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    for what in [
        "c",
        "C",
        "p,x=1,y=1",
        "q,x=1,y=1,z=0",
        "x,x=1",
        "y,y=1",
        "z,z=0",
        "n,I=1",
        "f",
        "r,x=1,y=2",
    ] {
        e.advance(&control(&format!("a=d,q=2,d={what}")));
    }
    assert_eq!(layout(&e).len(), 1);
    assert_eq!(store_counts(&e), (1, 1));
}

// ───────────────────────── 随 grid 走 ─────────────────────────

/// 视口算法:整张滚出视口就不交给绘制;翻回去又在。
#[test]
fn 滚出视口后不画_翻回来还在() {
    let e = terminal(40, 4);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.advance(b"\r\n\r\n\r\n");
    assert_eq!(layout(&e), vec![(0, 0, 2, 2)]);
    e.advance(b"\r\n");
    assert_eq!(layout(&e), vec![(-1, 0, 2, 2)], "还露着 1 行");
    e.advance(b"\r\n");
    assert!(layout(&e).is_empty(), "刚好整张出去");

    e.with_term_mut(|term| term.scroll_display(alacritty_terminal::grid::Scroll::Delta(1)));
    assert_eq!(layout(&e), vec![(-2, 0, 2, 2)], "翻回去一行,又露出来");
}

/// 有摆放但扫不到(整张都在视口外)时,每帧的结果是空而不是错位。
#[test]
fn 多张图按摆放先后排序() {
    let e = terminal(40, 10);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=1", &png(4, 4)));
    e.advance(b"\x1b[1;5H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=2", &png(4, 4)));
    e.advance(b"\x1b[3;1H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=3", &png(4, 4)));
    let uids: Vec<u64> = images(&e).iter().map(|image| image.bitmap.uid()).collect();
    assert!(uids.windows(2).all(|w| w[0] < w[1]), "{uids:?}");
    assert_eq!(uids.len(), 3);
}

/// 主屏 `CSI 2J`:协议口径删屏上的图,不让它跟进回看后下半截压在新屏幕上;数据留着。
#[test]
fn 清屏删屏上的图() {
    let e = terminal(40, 6);
    e.advance(b"text\r\n");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=4,i=1", &png(4, 4)));
    e.advance(b"\x1b[2J");
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (1, 0), "数据留着");
    assert!(e.history_lines() > 0, "文字照 alacritty 的口径进了回看");
    e.with_term_mut(|term| term.scroll_display(alacritty_terminal::grid::Scroll::Top));
    assert!(layout(&e).is_empty(), "翻回去也没有");
}

/// 清屏只删屏上的,回看里的不动。
#[test]
fn 清屏不动回看里的图() {
    let e = terminal(40, 4);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=1", &png(4, 4)));
    e.advance(b"\r\n\r\n\r\n\r\n\r\n");
    e.advance(b"\x1b[2J");
    assert_eq!(store_counts(&e).1, 1);
    e.with_term_mut(|term| term.scroll_display(alacritty_terminal::grid::Scroll::Top));
    assert_eq!(layout(&e).len(), 1);
}

/// `CSI 3J`:回看里的锚没了,对应摆放按死的处理;屏上的不动。
#[test]
fn 清回看后锚没了就不画() {
    let e = terminal(40, 4);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=1", &png(4, 4)));
    e.advance(b"\r\n\r\n\r\n\r\n\r\n");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=2", &png(4, 4)));
    e.advance(b"\x1b[3J");
    assert_eq!(layout(&e).len(), 1);
    e.with_term_mut(|term| term.scroll_display(alacritty_terminal::grid::Scroll::Top));
    assert_eq!(layout(&e).len(), 1, "翻不回去了,看到的还是屏上那张");
}

/// RIS 全清。
#[test]
fn 复位清空图片() {
    let e = terminal(40, 6);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.advance(b"\x1bc");
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (0, 0));
}

/// 备用屏:两块屏的图各画各的,`d=a` 与清死摆放都不跨屏。
#[test]
fn 备用屏互不影响() {
    let e = terminal(40, 6);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    e.advance(b"\x1b[?1049h");
    assert!(layout(&e).is_empty(), "主屏的图不画在备用屏上");

    e.advance(b"\x1b[3;1H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=2", &png(4, 4)));
    assert_eq!(layout(&e), vec![(2, 0, 2, 2)]);

    e.advance(&control("a=d,d=a,q=2"));
    assert!(layout(&e).is_empty());
    assert_eq!(store_counts(&e), (2, 1), "主屏那处还在");

    e.advance(b"\x1b[?1049l");
    assert_eq!(layout(&e), vec![(0, 0, 2, 2)], "回到主屏,图还在");
}

/// 备用屏上的图:退出之后不画,再进去时那块屏已经被清空,不复活。
#[test]
fn 备用屏的图退出后不复活() {
    let e = terminal(40, 6);
    e.advance(b"\x1b[?1049h");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    assert_eq!(layout(&e).len(), 1);
    e.advance(b"\x1b[?1049l");
    assert!(layout(&e).is_empty());
    e.advance(b"\x1b[?1049h");
    assert!(layout(&e).is_empty());
}

/// 重排:摆在折行正文中间的图跟着字走;列数变化不丢。
#[test]
fn 重排时图跟着格子走() {
    let e = terminal(10, 6);
    e.advance(b"0123456789abcde");
    e.advance(b"\x1b[2;3H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=1,i=1", &png(4, 4)));
    assert_eq!(layout(&e), vec![(1, 2, 2, 1)]);
    e.resize(TermSize::new(20, 6));
    assert_eq!(layout(&e), vec![(0, 12, 2, 1)]);
}

// ───────────────────────── 预算 ─────────────────────────

fn budgeted(budget: Budget) -> TerminalEmulator {
    let mut e = TerminalEmulator::with_scrollback(TermSize::new(40, 10), 100);
    let kitty = Kitty::with_budget(e.events.clone(), budget);
    kitty.set_cell_pixels(10.0, 20.0);
    e.parser.get_mut().cluster.set_kitty(kitty.shared());
    e.kitty = Some(kitty);
    e
}

/// 总字节超预算:先请屏外的,屏上和刚传的留着;字节数回到预算内。
#[test]
fn 超预算时淘汰且优先不可见的() {
    let side = 8u32;
    let one = u64::from(side * side * 4);
    let e = budgeted(Budget {
        bytes: one * 2,
        ..Budget::DEFAULT
    });
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(side, side))); // 没摆放
    e.advance(&apc("a=T,f=100,q=2,c=1,r=1,i=2", &png(side, side))); // 屏上
    assert_eq!(store_counts(&e).0, 2);

    e.advance(b"\x1b[2;1H");
    e.advance(&apc("a=T,f=100,q=2,c=1,r=1,i=3", &png(side, side)));
    let store = e.kitty().unwrap().shared.store.lock();
    assert_eq!(store.image_count(), 2);
    assert!(store.bytes() <= one * 2);
    drop(store);
    assert_eq!(layout(&e).len(), 2, "没摆放的那张先走,屏上两张都在");
}

/// 一张图都放不下预算时,队列预留挡住它;应答 ENOMEM,不影响后面。
#[test]
fn 单张超过总预算时拒收() {
    let e = budgeted(Budget {
        bytes: 16,
        ..Budget::DEFAULT
    });
    e.advance(&apc("a=T,f=100,q=1,c=1,r=1,i=1", &png(8, 8)));
    assert_eq!(store_counts(&e), (0, 0));
    assert_eq!(
        replies(&e),
        ["\x1b_Gi=1;ENOMEM:too many pending images\x1b\\"]
    );
    e.advance(&apc("a=T,f=100,q=2,c=1,r=1,i=2", &png(2, 2)));
    assert_eq!(store_counts(&e), (1, 1));
}

/// 同步块一直不结束时,攒在队列里的位图也算预算,撑不爆。
#[test]
fn 同步块里排队的位图也算预算() {
    let side = 8u32;
    let one = u64::from(side * side * 4);
    let e = budgeted(Budget {
        bytes: one * 2,
        ..Budget::DEFAULT
    });
    e.advance(BSU);
    for id in 1..=5 {
        e.advance(&apc(
            &format!("a=T,f=100,q=2,c=1,r=1,i={id}"),
            &png(side, side),
        ));
    }
    let shared = e.kitty().unwrap().shared();
    assert_eq!(shared.pending.lock().len(), 2, "多的直接拒收");
    assert_eq!(shared.pending_bytes.load(Ordering::Relaxed), one * 2);

    e.advance(ESU);
    assert_eq!(store_counts(&e).0, 2);
    assert_eq!(
        shared.pending_bytes.load(Ordering::Relaxed),
        0,
        "入库后还回来"
    );
}

/// 摆放太多:死的先清(锚被 `CSI K` 碾掉的),还多就撤最早的。
#[test]
fn 摆放个数有上限() {
    let e = budgeted(Budget {
        placements: 3,
        ..Budget::DEFAULT
    });
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(2, 2)));
    for row in 1..=6 {
        e.advance(format!("\x1b[{row};1H").as_bytes());
        e.advance(&control("a=p,q=2,i=1,c=1,r=1"));
    }
    assert!(store_counts(&e).1 <= 3);
    assert_eq!(layout(&e).len(), 3, "留下的都是最新的");
    assert_eq!(layout(&e).first().map(|l| l.0), Some(3));

    // 死的被清:碾掉一处之后再摆,不用撤活的
    let e = budgeted(Budget {
        placements: 2,
        ..Budget::DEFAULT
    });
    e.advance(&apc("a=t,f=100,q=2,i=1", &png(2, 2)));
    e.advance(&control("a=p,q=2,i=1,c=1,r=1"));
    e.advance(b"\x1b[2;1H");
    e.advance(&control("a=p,q=2,i=1,c=1,r=1"));
    e.advance(b"\x1b[1;1H\x1b[2K"); // 第一处的锚没了
    e.advance(b"\x1b[3;1H");
    e.advance(&control("a=p,q=2,i=1,c=1,r=1"));
    assert_eq!(
        layout(&e).iter().map(|l| l.0).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

/// 张数上限经 `a=t` 刷小图也撑不爆。
#[test]
fn 张数上限() {
    let e = budgeted(Budget {
        images: 4,
        ..Budget::DEFAULT
    });
    for id in 1..=20 {
        e.advance(&apc(&format!("a=t,f=100,q=2,i={id}"), &png(1, 1)));
    }
    assert_eq!(store_counts(&e).0, 4);
}

/// 终端释放时全部释放:位图没有别的持有者。
#[test]
fn 终端释放时位图跟着释放() {
    let e = terminal(40, 6);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    let bitmap = images(&e)[0].bitmap.clone();
    assert!(Arc::strong_count(&bitmap) >= 2);
    drop(e);
    assert_eq!(Arc::strong_count(&bitmap), 1, "只剩手上这一份");
}

/// 同一个 id 反复重发不累积。
#[test]
fn 重发不累积() {
    let e = terminal(40, 6);
    for _ in 0..50 {
        e.advance(&control("a=d,d=I,i=1,q=2"));
        e.advance(b"\x1b[1;1H\x1b[2K");
        e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=1", &png(4, 4)));
    }
    assert_eq!(store_counts(&e), (1, 1));
    assert_eq!(e.kitty().unwrap().shared.store.lock().bytes(), 4 * 4 * 4);
}

/// 没 id 的图:分内部 id,靠 `d=a` 收拾。
#[test]
fn 没给_id_的图() {
    let e = terminal(40, 6);
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2", &png(4, 4)));
    assert_eq!(layout(&e).len(), 1);
    e.advance(b"\x1b[3;1H");
    e.advance(&apc("a=T,f=100,q=2,c=2,r=2,i=0", &png(4, 4)));
    assert_eq!(layout(&e).len(), 2, "i=0 与没给同义");
    assert_eq!(store_counts(&e), (2, 2));
    e.advance(&control("a=d,d=A,q=2"));
    assert_eq!(store_counts(&e), (0, 0));
}

// ───────────────────────── 应答 ─────────────────────────

#[test]
fn 应答口径() {
    let e = terminal(40, 6);
    let cases: [(&str, &[&str]); 4] = [
        ("a=T,f=100,c=1,r=1,i=1", &["\x1b_Gi=1;OK\x1b\\"]),
        ("a=T,f=100,c=1,r=1,i=2,q=1", &[]),
        ("a=T,f=100,c=1,r=1,i=3,q=2", &[]),
        ("a=T,f=100,c=1,r=1", &[]),
    ];
    for (head, expect) in cases {
        e.advance(&apc(head, &png(2, 2)));
        assert_eq!(replies(&e), expect, "{head}");
    }
    // 错误:q=1 还答,q=2 不答
    e.advance(&apc("a=T,f=100,i=9,q=1", b"not a png"));
    assert_eq!(replies(&e), ["\x1b_Gi=9;EBADPNG:cannot decode\x1b\\"]);
    e.advance(&apc("a=T,f=100,i=9,q=2", b"not a png"));
    assert!(replies(&e).is_empty());
    // 带 p 的应答把 p 也带上(规范)
    e.advance(&apc("a=T,f=100,c=1,r=1,i=4,p=5", &png(2, 2)));
    assert_eq!(replies(&e), ["\x1b_Gi=4,p=5;OK\x1b\\"]);
    // 不支持的格式
    e.advance(&apc("a=T,f=24,s=1,v=1,i=8", &[0, 0, 0]));
    assert_eq!(
        replies(&e),
        ["\x1b_Gi=8;ENOTSUPPORTED:only PNG (f=100)\x1b\\"]
    );
}

/// 应答关着(Windows 的缺省,见 `GRAPHICS_REPLIES`):成败都不回,图照常入库;
/// `CSI 16 t` 不受影响。
#[test]
fn 关掉应答时图形命令一律不回() {
    let e = terminal(40, 6);
    e.kitty()
        .expect("开着")
        .shared()
        .replies
        .store(false, Ordering::Relaxed);

    e.advance(&apc("a=T,f=100,c=1,r=1,i=1", &png(2, 2)));
    e.advance(&apc("a=T,f=100,i=9", b"not a png"));
    e.advance(&apc("a=T,f=24,s=1,v=1,i=8", &[0, 0, 0]));
    assert!(replies(&e).is_empty());
    assert_eq!(images(&e).len(), 1);

    e.advance(b"\x1b[16t");
    assert_eq!(replies(&e), ["\x1b[6;20;10t"]);
}

/// `a=q` 不答:客户端探测会得出「不支持」,pi 靠 `PI_IMAGE_PROTOCOL` 绕过探测。
#[test]
fn 查询不应答() {
    let e = terminal(40, 6);
    e.advance(&apc("a=q,i=31,s=1,v=1,f=32,t=d", &[0, 0, 0, 0]));
    assert!(replies(&e).is_empty());
    assert_eq!(store_counts(&e), (0, 0));
}

// ───────────────────────── 像素查询 ─────────────────────────

#[test]
fn 一格像素的应答() {
    let e = terminal(40, 6);
    e.kitty().unwrap().set_cell_pixels(12.3, 24.0);
    e.advance(b"\x1b[16t");
    assert_eq!(replies(&e), ["\x1b[6;24;12t"]);
    assert_eq!(e.kitty().unwrap().reported_cell_pixels(), (12, 24));
    // 字号或缩放变了,下一次查询就是新值
    e.kitty().unwrap().set_cell_pixels(9.6, 19.5);
    e.advance(b"\x1b[16t");
    assert_eq!(replies(&e), ["\x1b[6;20;10t"]);
}

/// `CSI 14 t` 走 vte 的事件,宿主拿 [`Kitty::reported_cell_pixels`] 填。
#[test]
fn 文本区像素的应答() {
    let e = terminal(40, 6);
    e.kitty().unwrap().set_cell_pixels(10.0, 20.0);
    e.advance(b"\x1b[14t");
    let size = e.term_size();
    let (cell_width, cell_height) = e.kitty().unwrap().reported_cell_pixels();
    let answer: Vec<String> = e
        .events()
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            Event::TextAreaSizeRequest(format) => {
                Some(format(alacritty_terminal::event::WindowSize {
                    num_lines: size.screen_lines as u16,
                    num_cols: size.columns as u16,
                    cell_width,
                    cell_height,
                }))
            }
            _ => None,
        })
        .collect();
    assert_eq!(answer, ["\x1b[4;120;400t"]);
}

// ───────────────────────── 代数与快路径 ─────────────────────────

/// 全是 payload 的一批不进 term 锁,也不动内容代数。
#[test]
fn 纯_payload_不动代数() {
    let e = terminal(40, 6);
    let whole = chunked("a=T,f=100,q=2,c=2,r=2,i=1", &png(64, 64), 32);
    let first_end = whole
        .windows(2)
        .position(|w| w == b"\x1b\\")
        .expect("第一块的结尾")
        + 2;
    let generation = e.generation();
    e.advance(&whole[..first_end]);
    assert_eq!(e.generation(), generation, "只有第一块,没有字节要进 vte");
    e.advance(&whole[first_end..]);
    assert!(e.generation() > generation, "最后一块留了标记,要走 vte");
}

/// 光标水位的逐字节采样预算不被图片 payload 吃掉。
#[test]
fn 水位预算不被图片吃掉() {
    let e = terminal(40, 6);
    e.advance(b"a\r\nb");
    e.arm_cursor_floor();
    e.advance(&chunked("a=T,f=100,q=2,c=2,r=2,i=1", &png(256, 256), 4096));
    let budget = e.cursor_floor.lock().as_ref().map(|floor| floor.budget);
    assert!(
        budget.is_some_and(|left| left > crate::FLOOR_SAMPLE_BUDGET - 256),
        "{budget:?}"
    );
    assert_eq!(e.take_cursor_floor(), Some(1));
}

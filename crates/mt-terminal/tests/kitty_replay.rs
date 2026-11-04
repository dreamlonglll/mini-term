//! 回放真实 pi-tui 抓包:pi-tui 的 `Image` 组件 → 便携 ConPTY 1.24 → 这里录下的原始字节。
//!
//! 抓包有 1.2 MB,不进仓库。用 `MT_KITTY_PROBE_DIR=<目录> cargo test -p mt-terminal --test
//! kitty_replay -- --nocapture` 跑,目录里要有 `portable-1.24-kitty-small163KB.bin` 与
//! `portable-1.24-kitty-large940KB.bin`;没设就跳过。

use std::path::PathBuf;

use mt_terminal::alacritty_terminal::event::Event;
use mt_terminal::{TermSize, TerminalEmulator};

struct Capture {
    file: &'static str,
    image_id_rows: u32,
    source: (u32, u32),
    bitmap: (u32, u32),
    /// 抓包结束时光标停在哪一行(pi 退出时写了一个空格再换行)。
    final_row: i32,
}

/// 抓包时的探针环境:120×30,一格 11×23。
const CELL: (f32, f32) = (11.0, 23.0);

fn probe_dir() -> Option<PathBuf> {
    match std::env::var_os("MT_KITTY_PROBE_DIR") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            eprintln!("[kitty-replay] 没设 MT_KITTY_PROBE_DIR,跳过");
            None
        }
    }
}

fn replay(bytes: &[u8], chunk: usize) -> TerminalEmulator {
    let e =
        TerminalEmulator::with_scrollback(TermSize::new(120, 30), 1000).with_kitty_graphics(true);
    e.kitty().expect("开着").set_cell_pixels(CELL.0, CELL.1);
    for piece in bytes.chunks(chunk) {
        e.advance(piece);
    }
    e
}

fn check(capture: &Capture, bytes: &[u8], chunk: usize) {
    let e = replay(bytes, chunk);
    let images = e.with_term(|term| e.kitty().expect("开着").visible_images(term));
    assert_eq!(images.len(), 1, "切成 {chunk} 字节:应当正好一张图");
    let image = &images[0];
    assert_eq!(
        (image.line, image.column),
        (0, 0),
        "切成 {chunk} 字节:落在预留行的第一行"
    );
    assert_eq!((image.columns, image.rows), (40, capture.image_id_rows));
    assert_eq!(image.bitmap.source_size(), capture.source);
    assert_eq!(
        image.bitmap.size(),
        capture.bitmap,
        "切成 {chunk} 字节:按显示尺寸缩放"
    );
    let (width, height) = image.bitmap.size();
    assert_eq!(image.bitmap.bgra().len(), (width * height * 4) as usize);
    // 真解出了内容,不是一片同色(两张抓包都是界面截图)
    let (pixels, _) = image.bitmap.bgra().as_chunks::<4>();
    assert!(
        pixels.iter().any(|pixel| *pixel != pixels[0]),
        "位图是一片同色"
    );

    // payload 一个字节都没漏进 grid:全程只有 pi 退出时写的那一个空格
    assert!(
        e.visible_lines().iter().all(|line| line.is_empty()),
        "切成 {chunk} 字节:屏上有不该有的文字:\n{}",
        e.visible_lines().join("\n")
    );
    assert_eq!(
        e.with_term(|term| term.grid().cursor.point.line.0),
        capture.final_row,
        "切成 {chunk} 字节"
    );

    // `CSI 16 t` 只出现一次,应答一次,报的是渲染层给的数
    let replies: Vec<String> = e
        .events()
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            Event::PtyWrite(text) if text.starts_with("\x1b[6;") => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(replies, ["\x1b[6;23;11t"], "切成 {chunk} 字节");
}

#[test]
fn 真实抓包回放() {
    let Some(dir) = probe_dir() else {
        return;
    };
    let captures = [
        Capture {
            file: "portable-1.24-kitty-small163KB.bin",
            image_id_rows: 6,
            source: (1736, 519),
            bitmap: (440, 138),
            final_row: 7,
        },
        Capture {
            file: "portable-1.24-kitty-large940KB.bin",
            image_id_rows: 10,
            source: (1920, 1032),
            bitmap: (440, 230),
            final_row: 11,
        },
    ];
    for capture in &captures {
        let bytes = std::fs::read(dir.join(capture.file)).expect("读抓包");
        // ConPTY 实际的读批次在几 KB 上下;再加上切得很碎的几档
        for chunk in [bytes.len(), 65536, 4096, 977, 13] {
            let started = std::time::Instant::now();
            check(capture, &bytes, chunk);
            eprintln!(
                "[kitty-replay] {} chunk={chunk} ok in {:?}",
                capture.file,
                started.elapsed()
            );
        }
    }
    // 一字节一批太慢,只跑小的那份
    let bytes = std::fs::read(dir.join(captures[0].file)).expect("读抓包");
    check(&captures[0], &bytes, 1);
}

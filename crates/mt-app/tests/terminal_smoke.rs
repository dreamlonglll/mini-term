//! 端到端冒烟:PTY → VT → grid,不起 GPUI 窗口。
//!
//! 渲染那一半没法在无头环境里验(窗口要人眼看),但**链路的下半截**可以:
//! 往真 PTY 里写一条命令,从 grid 里读回回显。这条断言一挂,说明
//! spawn / write / reader 线程 / VT 状态机 里有一环断了 —— 与渲染无关。
//!
//! 第二条测的是逐列对齐的**数据前提**:中英混排时宽字符占两列、后面的窄字符
//! 落在正确的列号上。渲染器把第 N 列画在 `N × cell_width`,所以只要列号对,
//! 画出来就对。

use std::sync::Arc;
use std::time::{Duration, Instant};

use mt_pty::{PtySession, PtySpawn};
use mt_terminal::{TermSize, TerminalEmulator};

/// 起一个 shell,把输出喂进 emulator。
fn spawn_shell(size: TermSize) -> (PtySession, Arc<TerminalEmulator>) {
    let emulator = Arc::new(TerminalEmulator::new(size));
    let spec = shell_spec(size);
    let pty = {
        let emulator = emulator.clone();
        PtySession::spawn(spec, move |bytes| emulator.advance(bytes)).expect("PTY 起不来")
    };
    (pty, emulator)
}

#[cfg(windows)]
fn shell_spec(size: TermSize) -> PtySpawn {
    PtySpawn {
        program: "powershell.exe".into(),
        // -NoProfile:别让用户 profile 的 banner / oh-my-posh 污染 grid
        args: vec!["-NoLogo".into(), "-NoProfile".into()],
        cwd: None,
        env: vec![("TERM".into(), "xterm-256color".into())],
        rows: size.screen_lines as u16,
        cols: size.columns as u16,
    }
}

#[cfg(not(windows))]
fn shell_spec(size: TermSize) -> PtySpawn {
    PtySpawn {
        program: "/bin/sh".into(),
        args: vec![],
        cwd: None,
        env: vec![("TERM".into(), "xterm-256color".into()), ("PS1".into(), "$ ".into())],
        rows: size.screen_lines as u16,
        cols: size.columns as u16,
    }
}

/// 轮询 grid 直到 `pred` 成立或超时。PTY 是异步的,没有别的等法。
fn wait_for<T>(
    emulator: &TerminalEmulator,
    timeout: Duration,
    mut pred: impl FnMut(&[String]) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let lines = emulator.visible_lines();
        if let Some(v) = pred(&lines) {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn pty_回显能从_grid_读回来() {
    let size = TermSize::new(100, 30);
    let (mut pty, emulator) = spawn_shell(size);

    // 等 shell 起来(提示符出现即可,内容不作要求)
    let ready = wait_for(&emulator, Duration::from_secs(20), |lines| {
        lines.iter().any(|l| !l.trim().is_empty()).then_some(())
    });
    assert!(ready.is_some(), "shell 20s 内没有任何输出");

    // 用一个不会出现在提示符里的标记串
    pty.write(b"echo MT_SMOKE_9F3A\r").expect("写 PTY 失败");

    // 回显 + 命令输出,两次出现;至少要看到「不是命令行本身」的那一次。
    let hit = wait_for(&emulator, Duration::from_secs(20), |lines| {
        let count = lines
            .iter()
            .filter(|l| l.contains("MT_SMOKE_9F3A"))
            .count();
        (count >= 2).then_some(count)
    });
    assert!(
        hit.is_some(),
        "20s 内没有从 grid 读回 echo 的输出,grid 现状:\n{}",
        emulator.visible_lines().join("\n")
    );

    let _ = pty.kill();
}

#[test]
fn 中英混排的列位置对得上() {
    // 这条不需要 PTY:直接把字节推进状态机,验的是 grid 的列语义。
    let emulator = TerminalEmulator::new(TermSize::new(40, 4));
    emulator.advance("你好abc世界\r\n".as_bytes());

    let rows = emulator.visible_columns();
    let first: Vec<(usize, char)> = rows[0]
        .iter()
        .copied()
        .filter(|(_, c)| *c != ' ')
        .collect();

    // 你(0-1) 好(2-3) a(4) b(5) c(6) 世(7-8) 界(9-10)
    assert_eq!(
        first,
        vec![
            (0, '你'),
            (2, '好'),
            (4, 'a'),
            (5, 'b'),
            (6, 'c'),
            (7, '世'),
            (9, '界'),
        ],
        "宽字符必须占两列,窄字符必须紧跟在后一列上"
    );

    // 同一行读回的文本形态也要正确(spacer 列不重复出字)
    assert_eq!(emulator.visible_lines()[0], "你好abc世界");
}

/// 多 pane 各自独立:两个 PTY 各写一条不同的命令,各自的 grid 只出现自己那条。
///
/// 这是分屏/多 tab 的地基 —— 一旦 emulator 或 reader 线程被共享错了(比如
/// 两个 pane 复用同一个 `TerminalEmulator`),这条会立刻抓到:两块 grid 会
/// 互相串字。渲染那半截仍然只能人眼验,数据这半截可以钉死。
#[test]
fn 两个_pane_的_grid_互不串字() {
    let size = TermSize::new(100, 30);
    let (mut pty_a, term_a) = spawn_shell(size);
    let (mut pty_b, term_b) = spawn_shell(size);

    for term in [&term_a, &term_b] {
        let ready = wait_for(term, Duration::from_secs(20), |lines| {
            lines.iter().any(|l| !l.trim().is_empty()).then_some(())
        });
        assert!(ready.is_some(), "shell 20s 内没有任何输出");
    }

    pty_a.write(b"echo MT_PANE_AAA1\r").expect("写 PTY A 失败");
    pty_b.write(b"echo MT_PANE_BBB2\r").expect("写 PTY B 失败");

    let hit_a = wait_for(&term_a, Duration::from_secs(20), |lines| {
        (lines.iter().filter(|l| l.contains("MT_PANE_AAA1")).count() >= 2).then_some(())
    });
    let hit_b = wait_for(&term_b, Duration::from_secs(20), |lines| {
        (lines.iter().filter(|l| l.contains("MT_PANE_BBB2")).count() >= 2).then_some(())
    });
    assert!(hit_a.is_some(), "A 的输出没回到 A 的 grid");
    assert!(hit_b.is_some(), "B 的输出没回到 B 的 grid");

    let lines_a = term_a.visible_lines().join("\n");
    let lines_b = term_b.visible_lines().join("\n");
    assert!(
        !lines_a.contains("MT_PANE_BBB2"),
        "B 的输出串进了 A 的 grid:\n{lines_a}"
    );
    assert!(
        !lines_b.contains("MT_PANE_AAA1"),
        "A 的输出串进了 B 的 grid:\n{lines_b}"
    );

    let _ = pty_a.kill();
    let _ = pty_b.kill();
}

#[test]
fn 宽字符换行时不会被劈成两半() {
    // 40 列的终端,第 39 列(0-based)放不下一个宽字符,应整体折到下一行。
    let emulator = TerminalEmulator::new(TermSize::new(40, 4));
    let mut s = "x".repeat(39);
    s.push('中');
    emulator.advance(s.as_bytes());

    let rows = emulator.visible_columns();
    let second_row_first = rows[1].iter().find(|(_, c)| *c != ' ').copied();
    assert_eq!(
        second_row_first,
        Some((0, '中')),
        "宽字符在行尾放不下时应整体挪到下一行行首"
    );
}

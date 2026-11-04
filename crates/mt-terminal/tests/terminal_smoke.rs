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
use mt_terminal::alacritty_terminal::event::Event;
use mt_terminal::{SyncRelease, TermSize, TerminalEmulator};

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
        env: vec![
            ("TERM".into(), "xterm-256color".into()),
            ("PS1".into(), "$ ".into()),
        ],
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
        let count = lines.iter().filter(|l| l.contains("MT_SMOKE_9F3A")).count();
        (count >= 2).then_some(count)
    });
    assert!(
        hit.is_some(),
        "20s 内没有从 grid 读回 echo 的输出,grid 现状:\n{}",
        emulator.visible_lines().join("\n")
    );

    let _ = pty.kill();
}

/// 替宿主回终端应答。便携 ConPTY 一起来就发 DA1,收不到应答后面的输出全卡着
/// (见 `mt_pty::conpty` 的模块注释);mini-term 里这是 pane 的 `drain_term_events`。
fn answer_queries(emulator: &TerminalEmulator, pty: &PtySession) {
    for event in emulator.events().drain() {
        if let Event::PtyWrite(text) = event {
            pty.write_reply(text.as_bytes()).expect("写 PTY 失败");
        }
    }
}

/// 轮询 `pred` 直到成立或超时,期间替宿主回终端应答。
fn pump_until(
    emulator: &TerminalEmulator,
    pty: &PtySession,
    timeout: Duration,
    mut pred: impl FnMut(&TerminalEmulator) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        answer_queries(emulator, pty);
        if pred(emulator) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 可视区里 `needle` 出现在几行上。
fn lines_with(emulator: &TerminalEmulator, needle: &str) -> usize {
    let lines = emulator.visible_lines();
    lines.iter().filter(|l| l.contains(needle)).count()
}

/// 替宿主跑「到点释放」:mini-term 里这是 pane 的 `arm_sync_release`。
/// 真等到 deadline,走的是宿主调的那个入口。返回是否真的释放过。
fn release_when_due(emulator: &TerminalEmulator, pty: &PtySession, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        answer_queries(emulator, pty);
        match emulator.release_expired_sync() {
            SyncRelease::Released => return true,
            SyncRelease::Pending(due) => {
                std::thread::sleep(due.saturating_duration_since(Instant::now()))
            }
            SyncRelease::Idle => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    false
}

/// 这条用例对着哪个 ConPTY 跑,打在日志里。
///
/// 默认走系统 ConPTY。发行版默认用的是便携 ConPTY,想对着它跑:
/// `MT_SMOKE_CONPTY_DIR=<含 portable-conpty 的目录> cargo test -p mt-terminal --test
/// terminal_smoke <用例名> -- --nocapture`。预载要早于进程里任何一次 `openpty`,
/// 所以一次只跑一条。
fn announce_conpty_backend() {
    let backend = match std::env::var_os("MT_SMOKE_CONPTY_DIR") {
        Some(dir) => format!("{:?}", mt_pty::conpty::initialize(dir.as_ref())),
        None => "默认(系统 ConPTY)".to_string(),
    };
    eprintln!("[smoke] ConPTY 后端: {backend}");
}

/// OSC 8 穿过真 PTY 落到 cell 上:ConPTY 夹在中间,不是所有版本都原样放行。
#[test]
fn osc8_穿过真_pty_认得出网址() {
    announce_conpty_backend();
    let (mut pty, emulator) = spawn_shell(TermSize::new(100, 30));
    let ready = pump_until(&emulator, &pty, Duration::from_secs(20), |e| {
        e.visible_lines().iter().any(|l| !l.trim().is_empty())
    });
    assert!(ready, "shell 20s 内没有任何输出");

    // 标记串是算出来的,理由同下面那条
    const MARK: &str = "MT_LINK_1234";
    #[cfg(windows)]
    let command = "[Console]::Out.Write(\"$([char]27)]8;;https://example.com/smoke$([char]27)\\MT_LINK_\" + (1000 + 234) + \"$([char]27)]8;;$([char]27)\\`r`n\")\r";
    #[cfg(not(windows))]
    let command = "printf '\\033]8;;https://example.com/smoke\\033\\\\MT_LINK_%d\\033]8;;\\033\\\\\\n' $((1000 + 234))\r";
    pty.write(command.as_bytes()).expect("写 PTY 失败");
    let shown = pump_until(&emulator, &pty, Duration::from_secs(20), |e| {
        lines_with(e, MARK) > 0
    });
    assert!(
        shown,
        "20s 内没有输出,grid 现状:\n{}",
        emulator.visible_lines().join("\n")
    );

    let lines = emulator.visible_lines();
    let row = lines.iter().position(|l| l.contains(MARK)).unwrap();
    let column = emulator.visible_columns()[row]
        .windows(MARK.len())
        .find(|cells| cells.iter().map(|&(_, c)| c).eq(MARK.chars()))
        .map(|cells| cells[0].0)
        .expect("标记串的列位置");
    match emulator.url_at(row as i32, column) {
        Some(link) => {
            assert_eq!(link.url, "https://example.com/smoke");
            assert!(link.from_osc8);
            assert_eq!(link.cells.len(), MARK.len());
        }
        // 旧版系统 conhost 重新渲染时把链接丢了:文字还在,只是点不了
        None if cfg!(windows) => eprintln!("[smoke] 这个 ConPTY 把 OSC 8 吞了"),
        None => panic!("PTY 不该吞转义序列"),
    }
    let _ = pty.kill();
}

/// 漏发同步结束标记的端到端:子进程开了同步块就不管了(TUI 崩溃的样子),shell 还活着。
/// 没人释放时提示符与回显全进缓冲、到期也出不来;释放之后画面回来,再敲命令也不用再释放。
#[test]
fn 漏发同步结束标记时到期释放() {
    announce_conpty_backend();
    let (mut pty, emulator) = spawn_shell(TermSize::new(100, 30));
    let dump = |e: &TerminalEmulator| e.visible_lines().join("\n");
    let ready = pump_until(&emulator, &pty, Duration::from_secs(20), |e| {
        e.visible_lines().iter().any(|l| !l.trim().is_empty())
    });
    assert!(ready, "shell 20s 内没有任何输出");

    // 标记串是算出来的:回显里只有算式,grid 里出现整串只能是命令的输出
    const MARK: &str = "MT_SYNC_1234";
    #[cfg(windows)]
    let command = "[Console]::Out.Write(\"$([char]27)[?2026hMT_SYNC_\" + (1000 + 234))\r";
    #[cfg(not(windows))]
    let command = "printf '\\033[?2026hMT_SYNC_%d' $((1000 + 234))\r";
    pty.write(command.as_bytes()).expect("写 PTY 失败");

    let settled = pump_until(&emulator, &pty, Duration::from_secs(20), |e| {
        e.sync_deadline().is_some() || lines_with(e, MARK) > 0
    });
    assert!(
        settled,
        "20s 内命令没有结果,grid 现状:\n{}",
        dump(&emulator)
    );
    let Some(deadline) = emulator.sync_deadline() else {
        // 旧版系统 conhost 自己解析 VT 再重新渲染,认不得的 ?2026 到不了这一层:这种后端
        // 上不会假死,没东西可测(CI 的 Windows runner 可能就是它)
        if !cfg!(windows) {
            panic!("PTY 不该吞转义序列");
        }
        eprintln!("[smoke] 这个 ConPTY 把同步块吞了,跳过");
        let _ = pty.kill();
        return;
    };
    assert_eq!(lines_with(&emulator, MARK), 0, "同步块里的字节不该上屏");

    // 假死的样子:过了 deadline 没人释放,输出照旧出不来,敲的命令连回显都没有
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()) * 2);
    assert_eq!(lines_with(&emulator, MARK), 0, "到期不会自己释放");
    const DURING: &str = "MT_SYNC_DURING_55";
    pty.write(format!("echo {DURING}\r").as_bytes())
        .expect("写 PTY 失败");
    let leaked = pump_until(&emulator, &pty, Duration::from_millis(300), |e| {
        lines_with(e, DURING) > 0
    });
    assert!(!leaked, "还没释放,回显不该上屏:\n{}", dump(&emulator));

    // 释放:攒下的内容一起出来(DURING 回显与输出各一行)
    assert!(
        release_when_due(&emulator, &pty, Duration::from_secs(5)),
        "同步块一直没到期"
    );
    let shown = pump_until(&emulator, &pty, Duration::from_secs(20), |e| {
        lines_with(e, MARK) > 0 && lines_with(e, DURING) >= 2
    });
    assert!(shown, "释放之后内容没上屏,grid 现状:\n{}", dump(&emulator));

    // 释放之后 shell 是活的,回显与输出直接上屏,不用再释放
    const AFTER: &str = "MT_SYNC_AFTER_77";
    pty.write(format!("echo {AFTER}\r").as_bytes())
        .expect("写 PTY 失败");
    let alive = pump_until(&emulator, &pty, Duration::from_secs(20), |e| {
        lines_with(e, AFTER) >= 2
    });
    assert!(
        alive,
        "释放之后敲命令没有回显,grid 现状:\n{}",
        dump(&emulator)
    );
    assert_eq!(emulator.sync_deadline(), None);

    let _ = pty.kill();
}

#[test]
fn 中英混排的列位置对得上() {
    // 这条不需要 PTY:直接把字节推进状态机,验的是 grid 的列语义。
    let emulator = TerminalEmulator::new(TermSize::new(40, 4));
    emulator.advance("你好abc世界\r\n".as_bytes());

    let rows = emulator.visible_columns();
    let first: Vec<(usize, char)> = rows[0].iter().copied().filter(|(_, c)| *c != ' ').collect();

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

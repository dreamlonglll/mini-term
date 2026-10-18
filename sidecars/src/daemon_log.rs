//! daemon_log —— mt-ssh-cli daemon 的日志文件:位置、轮转、带 UTC 时间戳的事件行。
//!
//! 为什么要有:daemon 由 CLI detached 拉起,此前 stdio 全接 NUL —— 会话池的
//! `[mt-ssh] host key rejected: …`、重试日志与 daemon 自己的生命周期事件全丢,连接
//! 问题无从排查。现在 CLI 拉起 daemon 时把它的 stdout/stderr 接到数据目录下的
//! [`LOG_FILE_NAME`](见 [`crate::daemon_spawn`]),daemon 里一切 `eprintln!` 都落进
//! 这份文件:
//!
//! - 位置:`mt_core::config_json_path()` 的父目录 —— 与审计日志 `ssh-mcp-audit.log`
//!   同一个锚点;目录不存在就创建,定位不到就退回 NUL(日志只是排障辅助,绝不阻塞拉起);
//! - 轮转:拉起前超过 [`ROTATE_THRESHOLD`] 就改名成 `.log.1`,只留一代。「看大小 → 改名 →
//!   打开」持 `<日志>.lock` 上的系统文件锁串行执行(见 [`open_for_daemon`]);
//! - 事件行:`<UTC 毫秒时间戳> [mt-ssh-cli daemon] <事件>`。会话池等下层的 `eprintln!`
//!   不带时间戳,夹在这些事件行之间也能对上大致时刻。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// daemon 日志文件名(与 `config.json`、审计日志同目录)。
pub const LOG_FILE_NAME: &str = "mt-ssh-cli-daemon.log";

/// 拉起 daemon 前,日志超过这个字节数就轮转成 `.log.1`。
pub const ROTATE_THRESHOLD: u64 = 1024 * 1024;

/// 等轮转锁的上限。临界区只是「看大小 → 改名 → 打开」,正常几毫秒就让出来;等不到(不该
/// 发生)就这次不轮转、直接追加 —— 日志只是排障辅助,绝不阻塞拉起。
const ROTATE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// 等轮转锁时的轮询间隔。
const ROTATE_LOCK_POLL: Duration = Duration::from_millis(10);

/// daemon 日志的完整路径;数据目录定位不到时为 `None`。
pub fn log_path() -> Option<PathBuf> {
    let config = mt_core::config_json_path()?;
    Some(config.parent()?.join(LOG_FILE_NAME))
}

/// 轮转留下的上一代:`<日志>.1`。
pub fn rotated_path(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// 日志超过 `threshold` 字节就改名成 `.1`(覆盖更早的一代),返回是否轮转了。
/// 文件不存在不算错误。
pub fn rotate_if_oversized(log: &Path, threshold: u64) -> std::io::Result<bool> {
    match std::fs::metadata(log) {
        Ok(meta) if meta.len() > threshold => {
            std::fs::rename(log, rotated_path(log))?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// 轮转锁文件:`<日志>.lock`(空文件,常驻;锁是系统文件锁,不靠文件在不在)。
pub fn rotation_lock_path(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// 拿轮转锁:在锁文件上加独占的系统文件锁(Windows `LockFileEx`、Unix `flock`),最多等
/// `wait`。返回的句柄一关(drop),锁就放了;持锁进程崩溃时系统同样会放锁,不存在陈旧锁。
/// 打不开锁文件或等不到锁时返回 `None`。
fn lock_rotation(log: &Path, wait: Duration) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(rotation_lock_path(log))
        .ok()?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(ROTATE_LOCK_POLL);
            }
            Err(_) => return None,
        }
    }
}

/// 为即将拉起的 daemon 打开 `log` (追加写):建目录 → 持轮转锁按需轮转 → 打开。
///
/// 「看大小 → 改名 → 打开」三步必须串行:并发冷启动的几个 CLI 各自轮转时,后一个看到的
/// 还是旧大小,会把前一个刚建好、前一个 daemon 已经开着的新日志再改名成 `.1` —— 真正的上一代
/// 被冲掉,前一个 daemon 的句柄也跟着去了 `.1`。持锁后,后来者看到的已是新日志的大小。
/// 拿不到锁就这次不轮转(不持锁轮转正是那个竞态),只追加。
///
/// 轮转失败不影响继续追加(例如旧 daemon 还开着 `.1`,改名会被拒),只是这次不轮转。
pub fn open_for_daemon(log: &Path) -> std::io::Result<std::fs::File> {
    open_for_daemon_with(log, ROTATE_THRESHOLD, ROTATE_LOCK_WAIT)
}

/// [`open_for_daemon`] 的本体;阈值与等锁上限可注入(测试用)。
fn open_for_daemon_with(
    log: &Path,
    threshold: u64,
    lock_wait: Duration,
) -> std::io::Result<std::fs::File> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = lock_rotation(log, lock_wait);
    if lock.is_some() {
        let _ = rotate_if_oversized(log, threshold);
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log);
    // 锁持到日志打开之后才放。
    drop(lock);
    opened
}

/// 把 UNIX 纪元起的时长格式化成 `YYYY-MM-DDTHH:MM:SS.mmmZ`(纯函数,UTC)。
pub fn format_utc_millis(since_epoch: Duration) -> String {
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
    )
}

/// 1970-01-01 起的天数 → 公历 (年, 月, 日)。Howard Hinnant 的 `civil_from_days`,
/// 闰年规则全覆盖,不用逐年循环。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11],从三月起算
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// 当前 UTC 时间戳(毫秒精度)。系统时钟早于纪元时返回占位串。
pub fn utc_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(format_utc_millis)
        .unwrap_or_else(|_| "????-??-??T??:??:??.???Z".to_string())
}

/// 一行 daemon 事件日志(不含换行),纯函数。
pub fn event_line(timestamp: &str, message: &str) -> String {
    format!("{timestamp} [mt-ssh-cli daemon] {message}")
}

/// 往 stderr 记一条带时间戳的 daemon 事件。
///
/// CLI 拉起的 daemon 的 stderr 接在日志文件上;前台跑 `mt-ssh-cli daemon` 时打在终端。
/// 整行(含换行)一次写出:并发冷启动时几个 daemon 同时往同一份追加写的日志里写,
/// `eprintln!` 会把正文和换行拆成两次写,行与行就交错了(实测过)。
pub fn event(message: &str) {
    use std::io::Write;
    let mut line = event_line(&utc_now(), message);
    line.push('\n');
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> String {
        format_utc_millis(Duration::from_millis(millis))
    }

    #[test]
    fn utc_formatting_matches_reference_instants() {
        // 参考值取自 JS `new Date(ms).toISOString()`。
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(at(1_234_567_890_999), "2009-02-13T23:31:30.999Z");
        assert_eq!(at(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(at(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        assert_eq!(at(1_798_761_599_999), "2026-12-31T23:59:59.999Z");
        assert_eq!(at(4_102_444_800_000), "2100-01-01T00:00:00.000Z");
    }

    #[test]
    fn utc_now_has_expected_shape() {
        let now = utc_now();
        assert_eq!(now.len(), 24, "got: {now}");
        assert!(now.ends_with('Z'));
        assert_eq!(&now[10..11], "T");
        assert_eq!(&now[19..20], ".");
    }

    #[test]
    fn event_line_prefixes_timestamp_and_tag() {
        assert_eq!(
            event_line("2026-10-18T01:02:03.004Z", "idle, exiting"),
            "2026-10-18T01:02:03.004Z [mt-ssh-cli daemon] idle, exiting"
        );
    }

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mt-ssh-cli-daemon-log-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotation_keeps_exactly_one_previous_generation() {
        let dir = scratch_dir("rotate");
        let log = dir.join(LOG_FILE_NAME);
        assert_eq!(rotated_path(&log), dir.join("mt-ssh-cli-daemon.log.1"));

        // 不存在 / 未超阈值:不动。
        assert!(!rotate_if_oversized(&log, 8).unwrap());
        std::fs::write(&log, b"12345678").unwrap();
        assert!(!rotate_if_oversized(&log, 8).unwrap());
        assert!(log.exists());

        // 超阈值:改名成 .1,原位置空出来。
        std::fs::write(&log, b"first-generation").unwrap();
        assert!(rotate_if_oversized(&log, 8).unwrap());
        assert!(!log.exists());
        assert_eq!(
            std::fs::read(rotated_path(&log)).unwrap(),
            b"first-generation"
        );

        // 再轮转一次:旧的 .1 被覆盖,只留一代。
        std::fs::write(&log, b"second-generation").unwrap();
        assert!(rotate_if_oversized(&log, 8).unwrap());
        assert_eq!(
            std::fs::read(rotated_path(&log)).unwrap(),
            b"second-generation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_for_daemon_creates_missing_dirs_and_appends() {
        use std::io::Write;

        let dir = scratch_dir("open");
        let log = dir
            .join("not-yet")
            .join("com.mini-term.app")
            .join(LOG_FILE_NAME);
        open_for_daemon(&log).unwrap().write_all(b"a\n").unwrap();
        open_for_daemon(&log).unwrap().write_all(b"b\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "a\nb\n");
        assert_eq!(
            rotation_lock_path(&log),
            log.with_file_name("mt-ssh-cli-daemon.log.lock")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F7:几个 CLI 同时冷启动、同时为 daemon 打开日志 —— 只轮转一次,上一代完整留在 `.1`,
    /// 每个 daemon 都写进新日志(没有谁的句柄被后来者改名带去 `.1`)。
    #[test]
    fn concurrent_cold_starts_rotate_once_and_keep_the_previous_generation() {
        use std::io::Write;
        use std::sync::{Arc, Barrier};

        const OPENERS: usize = 8;
        let dir = scratch_dir("race");
        let log = dir.join(LOG_FILE_NAME);
        for round in 0..20 {
            // 上一代:远超阈值;新写的几行加起来远低于阈值,不会二次触发轮转。
            let previous = format!("previous generation {round}\n").repeat(64);
            std::fs::write(&log, &previous).unwrap();
            let _ = std::fs::remove_file(rotated_path(&log));

            let barrier = Arc::new(Barrier::new(OPENERS));
            let openers: Vec<_> = (0..OPENERS)
                .map(|i| {
                    let (log, barrier) = (log.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        let mut file =
                            open_for_daemon_with(&log, 512, Duration::from_secs(10)).unwrap();
                        writeln!(file, "daemon-{i}").unwrap();
                    })
                })
                .collect();
            for opener in openers {
                opener.join().unwrap();
            }

            assert_eq!(
                std::fs::read_to_string(rotated_path(&log)).unwrap(),
                previous,
                "round {round}: the previous generation was overwritten"
            );
            let current = std::fs::read_to_string(&log).unwrap();
            for i in 0..OPENERS {
                assert!(
                    current.contains(&format!("daemon-{i}\n")),
                    "round {round}: daemon-{i} did not write into the current log:\n{current}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 别人一直占着轮转锁(不该发生):等到上限就这次不轮转,照常追加,不卡住拉起。
    #[test]
    fn rotation_is_skipped_when_the_lock_cannot_be_taken() {
        use std::io::Write;

        let dir = scratch_dir("locked");
        let log = dir.join(LOG_FILE_NAME);
        std::fs::write(&log, b"oversized-generation").unwrap();
        let held = lock_rotation(&log, Duration::ZERO).expect("take the lock");

        let started = Instant::now();
        let mut file = open_for_daemon_with(&log, 8, Duration::from_millis(100)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100));
        file.write_all(b"+appended").unwrap();
        drop(file);
        assert!(
            !rotated_path(&log).exists(),
            "must not rotate without the lock"
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "oversized-generation+appended"
        );

        // 锁放了之后照常轮转。
        drop(held);
        drop(open_for_daemon_with(&log, 8, Duration::from_millis(100)).unwrap());
        assert_eq!(
            std::fs::read_to_string(rotated_path(&log)).unwrap(),
            "oversized-generation+appended"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

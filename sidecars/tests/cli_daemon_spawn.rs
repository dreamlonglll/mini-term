//! mt-ssh-cli 冷启动拉起 daemon 的端到端回归(仅 Windows)。
//!
//! 钉住三件实测踩过的事:
//! - daemon 不继承 CLI 的标准句柄:CLI 一退出,接它 stdout/stderr 的管道就该 EOF。此前
//!   daemon 拿着管道写端,`$(cli …)` / `cli … | tail` 要等它空闲自退(10 分钟)才结束;
//! - daemon 不继承 CLI 的工作目录:CLI 退出后,调用方的当前目录能被改名;
//! - daemon 的 stderr 落进数据目录下的日志文件,`daemon-status` 报得出日志路径。
//!
//! 隔离:独立端点(`MINITERM_SSH_CLI_ENDPOINT`)+ 临时 `APPDATA`,不碰真实 daemon 与真实
//! 配置。测试拉起的 daemon 跑的是 target 里的 exe,不停掉会锁住它、让下次构建失败 ——
//! 收尾无论成败都 `daemon-stop` 并等进程真正退出。所有等待都有上限:回归时报失败,
//! 不会挂死。
#![cfg(windows)]

use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

const CLI: &str = env!("CARGO_BIN_EXE_mt-ssh-cli");

/// CLI 退出后,接它输出的管道最多再等这么久就必须 EOF。
const EOF_GRACE: Duration = Duration::from_secs(5);

/// 隔离沙箱:独立端点 + 临时 APPDATA + 调用方的工作目录。
struct Sandbox {
    root: PathBuf,
    appdata: PathBuf,
    work: PathBuf,
    endpoint: String,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tag = format!("{label}-{}-{nanos}", std::process::id());
        let root = std::env::temp_dir().join(format!("mt-ssh-cli-it-{tag}"));
        let appdata = root.join("appdata");
        let work = root.join("work");
        std::fs::create_dir_all(&appdata).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Self {
            root,
            appdata,
            work,
            endpoint: format!(r"\\.\pipe\mt-cli-it-{tag}"),
        }
    }

    /// 带隔离环境的 CLI 命令。
    fn cli(&self) -> Command {
        let mut cmd = Command::new(CLI);
        cmd.env("APPDATA", &self.appdata)
            .env("MINITERM_SSH_CLI_ENDPOINT", &self.endpoint)
            .env_remove("MT_APP_DATA_DIR");
        cmd
    }

    fn log_path(&self) -> PathBuf {
        self.appdata
            .join("com.mini-term.app")
            .join("mt-ssh-cli-daemon.log")
    }
}

/// 收尾守卫:不管测试成败都 `daemon-stop`,再等 daemon 进程真正退出(释放 target 里
/// exe 的文件锁);`daemon-stop` 不管用时对着已核对过映像路径的进程句柄强杀兜底。
struct DaemonCleanup<'a> {
    sandbox: &'a Sandbox,
    process: Option<OwnedHandle>,
}

impl<'a> DaemonCleanup<'a> {
    fn new(sandbox: &'a Sandbox) -> Self {
        Self {
            sandbox,
            process: None,
        }
    }

    /// 记下 daemon 进程。拿到 pid 立刻开句柄:之后的等待 / 兜底强杀都对着这个进程对象,
    /// 不受 pid 复用影响;并核对它确实是本测试的 mt-ssh-cli.exe。
    fn track(&mut self, pid: u32) {
        let access = PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
        // SAFETY: 失败返回 NULL;成功的句柄交给 OwnedHandle 关闭。
        let raw = unsafe { OpenProcess(access, 0, pid) };
        assert_ne!(raw, 0, "OpenProcess(pid {pid}) failed");
        // SAFETY: raw 是刚打开、无人共享的有效句柄。
        let process = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        let image = process_image(&process).expect("query daemon image path");
        assert!(
            same_file(&image, Path::new(CLI)),
            "pid {pid} is {} rather than the mt-ssh-cli under test",
            image.display()
        );
        self.process = Some(process);
    }
}

impl Drop for DaemonCleanup<'_> {
    fn drop(&mut self) {
        let _ = run_cli(
            self.sandbox.cli().arg("daemon-stop"),
            Duration::from_secs(15),
        );
        if let Some(process) = &self.process {
            let raw = process.as_raw_handle() as HANDLE;
            // SAFETY: raw 由 self.process 持有,此刻有效。
            unsafe {
                if WaitForSingleObject(raw, 10_000) != WAIT_OBJECT_0 {
                    TerminateProcess(raw, 1);
                    WaitForSingleObject(raw, 5_000);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.sandbox.root);
    }
}

/// 进程的映像全路径。
fn process_image(process: &OwnedHandle) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buf 可写 len 个 u16;成功后 len 是写入的字符数(不含 NUL)。
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle() as HANDLE,
            PROCESS_NAME_WIN32,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    (ok != 0).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize])))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

/// 在线程里把一个管道读到 EOF,读完经 channel 交回内容。
fn drain_to_eof(
    mut pipe: impl Read + Send + 'static,
    name: &'static str,
    tx: mpsc::Sender<(&'static str, Vec<u8>)>,
) {
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = pipe.read_to_end(&mut data);
        let _ = tx.send((name, data));
    });
}

/// 等子进程退出,最多 `limit`;超时返回 None(调用方负责收拾)。
fn wait_with_timeout(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 一次 CLI 调用的结果。
struct CliRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// 跑一条不会拉起 daemon 的 CLI 命令(`daemon-status` / `daemon-stop`),带超时。
fn run_cli(cmd: &mut Command, limit: Duration) -> std::io::Result<CliRun> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    drain_to_eof(child.stdout.take().unwrap(), "stdout", tx.clone());
    drain_to_eof(child.stderr.take().unwrap(), "stderr", tx);
    let status = wait_with_timeout(&mut child, limit);
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut run = CliRun {
        code: status.and_then(|s| s.code()),
        stdout: String::new(),
        stderr: String::new(),
    };
    for _ in 0..2 {
        match rx.recv_timeout(EOF_GRACE) {
            Ok(("stdout", data)) => run.stdout = String::from_utf8_lossy(&data).into_owned(),
            Ok((_, data)) => run.stderr = String::from_utf8_lossy(&data).into_owned(),
            Err(_) => break,
        }
    }
    Ok(run)
}

/// 从 `daemon-status` 的状态行里取 pid。
fn parse_pid(status_line: &str) -> Option<u32> {
    let rest = &status_line[status_line.find("pid=")? + 4..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[test]
fn cold_start_releases_caller_pipes_and_cwd_and_logs_to_the_data_dir() {
    let sandbox = Sandbox::new("cold");
    let mut cleanup = DaemonCleanup::new(&sandbox);

    // 1. 冷启动:端点上没有 daemon,CLI 会拉起一个。stdout/stderr 用管道接住 —— 正是
    //    `$(cli …)` / `cli … | tail` 的形态;cwd 设为「调用方的项目目录」。
    let mut child = sandbox
        .cli()
        .args(["list", "--project-token", "bogus-token"])
        .current_dir(&sandbox.work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mt-ssh-cli");
    let (tx, rx) = mpsc::channel();
    drain_to_eof(child.stdout.take().unwrap(), "stdout", tx.clone());
    drain_to_eof(child.stderr.take().unwrap(), "stderr", tx);
    let Some(status) = wait_with_timeout(&mut child, Duration::from_secs(60)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("mt-ssh-cli list did not exit within 60s");
    };

    // 2. CLI 已退出:两个管道都必须很快 EOF。
    let deadline = Instant::now() + EOF_GRACE;
    let mut closed = Vec::new();
    let mut stderr = String::new();
    while closed.len() < 2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((name, data)) => {
                if name == "stderr" {
                    stderr = String::from_utf8_lossy(&data).into_owned();
                }
                closed.push(name);
            }
            Err(_) => panic!(
                "pipes still open {EOF_GRACE:?} after mt-ssh-cli exited (closed so far: \
                 {closed:?}) — the daemon inherited the caller's stdout/stderr handles"
            ),
        }
    }
    // 业务结果:请求确实经新拉起的 daemon 处理(没走降级),假令牌被拒、exit 2。
    assert_eq!(status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("invalid project token"), "stderr: {stderr}");
    assert!(
        !stderr.contains("falling back"),
        "the request must go through the spawned daemon; stderr: {stderr}"
    );

    // 3. daemon 仍在跑(确实走了冷启动拉起),记下它供收尾;状态行带在途数与日志路径。
    let status = run_cli(sandbox.cli().arg("daemon-status"), Duration::from_secs(15))
        .expect("run daemon-status");
    assert_eq!(
        status.code,
        Some(0),
        "daemon-status: {}{}",
        status.stdout,
        status.stderr
    );
    cleanup.track(parse_pid(&status.stdout).expect("pid in daemon-status output"));
    assert!(status.stdout.contains(" active="), "{}", status.stdout);
    let log = sandbox.log_path();
    assert!(
        status.stdout.contains(&log.display().to_string()),
        "daemon-status must print the log path: {}",
        status.stdout
    );

    // 4. 调用方的工作目录没被 daemon 占着:CLI 退出后可以改名。
    let renamed = sandbox.root.join("work-renamed");
    if let Err(e) = std::fs::rename(&sandbox.work, &renamed) {
        panic!("the caller's working directory is still locked after the CLI exited: {e}");
    }
    std::fs::rename(&renamed, &sandbox.work).unwrap();

    // 5. daemon 的 stderr 落进了数据目录下的日志文件,带时间戳的生命周期事件可见。
    let text = std::fs::read_to_string(&log)
        .unwrap_or_else(|e| panic!("daemon log {} missing: {e}", log.display()));
    assert!(text.contains("[mt-ssh-cli daemon] starting v"), "{text}");
    assert!(
        text.contains(&format!("listening on {}", sandbox.endpoint)),
        "{text}"
    );
}

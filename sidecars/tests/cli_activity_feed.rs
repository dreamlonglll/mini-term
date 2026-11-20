//! mt-ssh-cli 浮层抄送(`MINITERM_SSH_FEED`,spec ssh-activity-overlay-v1)的端到端回归(仅 Windows)。
//!
//! 钉住 CLI 侧最要紧的两条:
//! - 抄送不改变 CLI 本身:同一条命令带 / 不带这个变量(含端口没人监听),stdout / stderr / 退出码
//!   逐字节一致;
//! - 只报 exec / upload / download:start 在入口发一次(降级 one-shot 不重发),CLI 自己的降级提示
//!   不镜像,受理之后的错误进 end.error;list、运维子命令、`run_plan` 之前的用法错误根本不连。
//!
//! 不拉起 daemon:端点(`MINITERM_SSH_CLI_ENDPOINT`)被一个拒绝本进程连接的管道占着,CLI 立即
//! 降级进程内执行(提权运行时它根本不碰 daemon,同样进程内执行);临时 `APPDATA` 是空的,业务请求
//! 在读配置那一步就以 exit 2 失败 —— 不碰网络、不碰真实配置。
#![cfg(windows)]

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mt_sidecars::activity_feed::FEED_ENV;
use mt_sidecars::ipc::windows_security::PipeSecurity;
use serde_json::Value;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

const CLI: &str = env!("CARGO_BIN_EXE_mt-ssh-cli");

const NONCE: &str = "00112233445566778899aabbccddeeff";

/// 一次 CLI 调用的上限:回归时报失败,不挂死。
const RUN_LIMIT: Duration = Duration::from_secs(60);

/// 隔离沙箱:临时 APPDATA + 被占住、拒绝本进程连接的 daemon 端点。
struct Sandbox {
    root: PathBuf,
    appdata: PathBuf,
    endpoint: String,
    // 字段按声明顺序析构:管道先于 runtime 关掉。
    _holder: NamedPipeServer,
    _security: PipeSecurity,
    _runtime: tokio::runtime::Runtime,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tag = format!("{label}-{}-{nanos}", std::process::id());
        let root = std::env::temp_dir().join(format!("mt-ssh-cli-feed-it-{tag}"));
        let appdata = root.join("appdata");
        std::fs::create_dir_all(&appdata).unwrap();
        let endpoint = format!(r"\\.\pipe\mt-cli-feed-it-{tag}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // 只授权 LocalSystem 的管道占住端点:本进程拉起的 CLI 连不上,也不拉起 daemon。
        let security = PipeSecurity::from_sddl("D:P(A;;GA;;;SY)").unwrap();
        let holder = runtime.block_on(async {
            // SAFETY: security 随沙箱存活。
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .create_with_security_attributes_raw(&endpoint, security.attributes_ptr())
            }
            .unwrap()
        });
        Self {
            root,
            appdata,
            endpoint,
            _holder: holder,
            _security: security,
            _runtime: runtime,
        }
    }

    /// 带隔离环境的 CLI 命令;`feed` 是 `MINITERM_SSH_FEED` 的值(`None` = 不设)。跑测试的 shell
    /// 自己的 MSYS 变量与浮层变量一律剔掉 —— 在 mini-term 的 pane 里跑 `cargo test` 时,不能把
    /// 基线调用报给真的 mini-term。
    fn cli(&self, feed: Option<&str>) -> Command {
        let mut cmd = Command::new(CLI);
        cmd.env("APPDATA", &self.appdata)
            .env("MINITERM_SSH_CLI_ENDPOINT", &self.endpoint)
            .env_remove("MT_APP_DATA_DIR")
            .env_remove("MSYSTEM")
            .env_remove("MSYS_NO_PATHCONV")
            .env_remove("MSYS2_ARG_CONV_EXCL")
            .env_remove(FEED_ENV)
            .current_dir(&self.root);
        if let Some(value) = feed {
            cmd.env(FEED_ENV, value);
        }
        cmd
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 假浮层:本机监听,收下每条连接并按行解析成帧。
struct FakeOverlay {
    listener: TcpListener,
}

impl FakeOverlay {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self { listener }
    }

    /// 指向本监听的 `MINITERM_SSH_FEED` 值。
    fn feed_value(&self) -> String {
        format!("{}/{NONCE}", self.listener.local_addr().unwrap())
    }

    /// 收下 `wait` 之内到达的全部连接,每条读到 EOF。CLI 退出前已等后台线程把 end 写完,
    /// 它的连接此刻已在 backlog 里、数据已在接收缓冲里。
    fn collect(&self, wait: Duration) -> Vec<Vec<Value>> {
        let deadline = Instant::now() + wait;
        let mut calls = Vec::new();
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Windows 上接受的 socket 继承监听 socket 的非阻塞模式。
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let frames = BufReader::new(stream)
                        .lines()
                        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
                        .collect();
                    calls.push(frames);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
        calls
    }
}

/// 一次 CLI 调用的结果(逐字节比较用)。
#[derive(Debug, PartialEq, Eq)]
struct CliRun {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CliRun {
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// 在线程里把一个管道读到 EOF。
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = pipe.read_to_end(&mut data);
        let _ = tx.send(data);
    });
    rx
}

fn wait_with_timeout(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run(cmd: &mut Command) -> CliRun {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mt-ssh-cli");
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let Some(status) = wait_with_timeout(&mut child, RUN_LIMIT) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("mt-ssh-cli did not exit within {RUN_LIMIT:?}");
    };
    CliRun {
        code: status.code(),
        stdout: stdout.recv_timeout(Duration::from_secs(5)).unwrap(),
        stderr: stderr.recv_timeout(Duration::from_secs(5)).unwrap(),
    }
}

/// end 帧里的错误原文,同时核对它就是 CLI 打在 stderr 上的那一行(去掉前缀)。
fn end_error(end: &Value, run: &CliRun) -> String {
    assert_eq!(end["type"], "end", "{end}");
    let error = end["error"]
        .as_str()
        .unwrap_or_else(|| panic!("end without error: {end}"))
        .to_string();
    assert!(
        run.stderr_text()
            .contains(&format!("mt-ssh-cli: error: {error}\n")),
        "end.error {error:?} is not what the CLI printed: {}",
        run.stderr_text()
    );
    error
}

#[test]
fn exec_reports_start_and_end_without_changing_cli_output() {
    let sandbox = Sandbox::new("exec");
    let overlay = FakeOverlay::new();
    let args = [
        "exec",
        "--project-token",
        "bogus-token",
        "--cwd",
        "~/app",
        "--timeout",
        "30",
        "prod",
        "--",
        "echo",
        "hi there",
    ];
    let baseline = run(sandbox.cli(None).args(args));
    let fed = run(sandbox.cli(Some(&overlay.feed_value())).args(args));
    assert_eq!(fed, baseline, "the feed must not change the CLI's output");
    // 降级 one-shot 后照常执行:临时 APPDATA 是空的,报「配置不存在」、exit 2。
    assert_eq!(baseline.code, Some(2), "{}", baseline.stderr_text());
    assert!(
        baseline.stderr_text().contains("does not exist"),
        "{}",
        baseline.stderr_text()
    );

    let calls = overlay.collect(Duration::from_millis(300));
    assert_eq!(calls.len(), 1, "one connection per call: {calls:?}");
    // 只有 start 与 end:降级提示不镜像,start 没因降级重发。
    let [start, end] = calls[0].as_slice() else {
        panic!("expected start + end, got {:?}", calls[0]);
    };
    assert_eq!(start["type"], "start");
    assert_eq!(start["v"], 1);
    assert_eq!(start["nonce"], NONCE);
    assert_eq!(start["op"], "exec");
    assert_eq!(start["connection"], "prod");
    assert_eq!(start["command"], "echo hi there");
    assert_eq!(start["cwd"], "~/app");
    assert_eq!(start["timeoutSecs"], 30);
    assert!(start.get("localPath").is_none(), "{start}");

    let error = end_error(end, &baseline);
    assert!(error.contains("does not exist"), "{error}");
    assert!(end.get("exitCode").is_none(), "{end}");
    assert_eq!(end["timedOut"], false);
    assert_eq!(end["outputBytes"], 0);
    assert_eq!(end["truncated"], false);
}

#[test]
fn transfers_report_both_paths_without_changing_cli_output() {
    let sandbox = Sandbox::new("transfer");
    let overlay = FakeOverlay::new();
    let cases: [(&str, [&str; 6], &str, &str); 2] = [
        (
            "upload",
            [
                "upload",
                "--project-token",
                "bogus-token",
                "prod",
                "local.txt",
                "/tmp/remote.txt",
            ],
            r"\local.txt",
            "/tmp/remote.txt",
        ),
        (
            "download",
            [
                "download",
                "--project-token",
                "bogus-token",
                "prod",
                "/var/log/app.log",
                "out/app.log",
            ],
            r"\out\app.log",
            "/var/log/app.log",
        ),
    ];
    for (op, args, local_suffix, remote) in cases {
        let baseline = run(sandbox.cli(None).args(args));
        let fed = run(sandbox.cli(Some(&overlay.feed_value())).args(args));
        assert_eq!(
            fed, baseline,
            "{op}: the feed must not change the CLI's output"
        );
        assert_eq!(baseline.code, Some(2), "{op}: {}", baseline.stderr_text());

        let calls = overlay.collect(Duration::from_millis(300));
        assert_eq!(calls.len(), 1, "{op}: {calls:?}");
        let [start, end] = calls[0].as_slice() else {
            panic!("{op}: expected start + end, got {:?}", calls[0]);
        };
        assert_eq!(start["op"], op);
        assert_eq!(start["connection"], "prod");
        assert_eq!(start["remotePath"], remote);
        // 本地路径是调用现场绝对化之后的。
        let local = start["localPath"].as_str().unwrap();
        assert!(Path::new(local).is_absolute(), "{op}: {local}");
        assert!(local.ends_with(local_suffix), "{op}: {local}");
        assert!(start.get("command").is_none(), "{op}: {start}");
        assert!(start.get("timeoutSecs").is_none(), "{op}: {start}");

        end_error(end, &baseline);
        assert!(end.get("bytes").is_none(), "{op}: {end}");
    }
}

#[test]
fn unreachable_overlay_changes_nothing() {
    let sandbox = Sandbox::new("closed");
    // 绑一个端口再放掉:之后没人监听。
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let args = [
        "exec",
        "--project-token",
        "bogus-token",
        "prod",
        "--",
        "uptime",
    ];
    let started = Instant::now();
    let baseline = run(sandbox.cli(None).args(args));
    let baseline_took = started.elapsed();
    let started = Instant::now();
    let fed = run(sandbox.cli(Some(&format!("{closed}/{NONCE}"))).args(args));
    let fed_took = started.elapsed();
    assert_eq!(fed, baseline, "the feed must not change the CLI's output");
    // 连接上限 200ms、收尾最多等 500ms:多出来的远不到秒级(余量留给进程启动的抖动)。
    assert!(
        fed_took < baseline_took + Duration::from_millis(1500),
        "baseline {baseline_took:?}, with an unreachable feed {fed_took:?}"
    );
}

#[test]
fn list_ops_commands_and_usage_errors_are_never_reported() {
    let sandbox = Sandbox::new("silent");
    let overlay = FakeOverlay::new();
    let feed = overlay.feed_value();

    // list 只读本地配置:带不带变量输出一致,且不连浮层。
    let list = ["list", "--project-token", "bogus-token"];
    let baseline = run(sandbox.cli(None).args(list));
    let fed = run(sandbox.cli(Some(&feed)).args(list));
    assert_eq!(fed, baseline);
    assert_eq!(baseline.code, Some(2), "{}", baseline.stderr_text());

    // daemon 运维子命令(端点拒绝本进程 → exit 2)。
    let status = run(sandbox.cli(Some(&feed)).arg("daemon-status"));
    assert_eq!(status.code, Some(2), "{}", status.stderr_text());

    // run_plan 之前的用法错误:clap(缺远程命令)与 Git Bash 改写过的实参。
    let clap =
        run(sandbox
            .cli(Some(&feed))
            .args(["exec", "--project-token", "bogus-token", "prod"]));
    assert_eq!(clap.code, Some(2), "{}", clap.stderr_text());
    let msys = run(sandbox.cli(Some(&feed)).env("MSYSTEM", "MINGW64").args([
        "exec",
        "--project-token",
        "bogus-token",
        "prod",
        "--",
        "tail",
        "D:/Program Files/Git/var/log/syslog",
    ]));
    assert_eq!(msys.code, Some(2), "{}", msys.stderr_text());
    assert!(
        msys.stderr_text().contains("Git Bash (MSYS)"),
        "{}",
        msys.stderr_text()
    );

    let calls = overlay.collect(Duration::from_millis(500));
    assert!(calls.is_empty(), "nothing may be reported: {calls:?}");
}

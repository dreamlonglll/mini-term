//! PTY 生命周期:spawn / read / write / resize / kill。**不含任何 UI 与 VT 解析**。
//!
//! # 从 `src-tauri/src/pty.rs` 移入的范围
//!
//! | 现有代码 | 去向 |
//! |---|---|
//! | `create_pty` / `write_pty` / `resize_pty` / `kill_pty` | 本 crate,去掉 `#[tauri::command]`,改成普通方法 |
//! | AI 命令识别(`claude`/`codex`/`opencode`/`pi`/`grok` + ↑ 历史/Tab 补全的行快照兜底与输出回扫) | `mt-ai`,它需要的只是「用户键入的字节」这一路输入 |
//! | 裸 Esc / Ctrl+C 的用户打断识别(`note_user_interrupt`) | `mt-ai`,同上 |
//! | `arm_ssh_autofill` | 保留在本 crate(它就是往 PTY 写字节) |
//! | ConPTY 便携 DLL 预载(`conpty_bootstrap.rs`) | 本 crate,原样搬,仍须早于任何 `openpty` |
//!
//! # 明确**不要**移过来的东西
//!
//! 以下代码在 GPUI 架构下没有存在意义,移植时直接删掉,不要试图保留:
//!
//! - **16ms 批量缓冲**:原本是为了摊薄 `emit('pty-output')` 的 IPC 开销。现在
//!   reader 线程读到的字节直接进 `mt-terminal` 的 grid,没有 IPC。
//! - **有界 channel + 4MB/1MB 双水位背压 + `set_pty_flow_paused` + 30s 超时兜底**:
//!   原本是拿来在 WebView 边界上人工造一条背压链路。现在解析速度就是本进程的
//!   速度,读慢了 ConPTY 自然阻塞刷屏进程,背压是天然的。
//! - **`kill_all_ptys` 孤儿回收**:原本是为了兜住 WebView2 renderer 被 OOM 杀掉后
//!   页面重载、旧 PTY 无人引用却继续运行。GPUI 是单进程,进程没了 PTY 也就没了。
//!
//! 这三块是本次改造在后端侧最大的一笔净删除,详见 `docs/gpui-migration.md`。

use std::io::{Read, Write};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// 一个活着的 PTY 会话。持有 master 端、子进程句柄和写入端。
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

/// 创建 PTY 所需的参数。字段刻意保持贫瘠 —— 现有 `create_pty` 的其余参数
/// (AI 识别相关、状态上报相关)属于 `mt-ai`,不该经过这里。
#[derive(Debug, Clone)]
pub struct PtySpawn {
    /// shell 可执行文件路径或名字。
    pub program: String,
    /// 传给 shell 的参数。
    pub args: Vec<String>,
    /// 工作目录;`None` 表示继承当前进程。
    pub cwd: Option<String>,
    /// 追加到子进程环境的键值对。
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

impl PtySession {
    /// 起一个 PTY 并 spawn 子进程。
    ///
    /// `on_output` 在**独立的 reader 线程**上被调用,每次拿到一段刚读出的字节。
    /// 调用方(`mt-terminal`)在这里把字节喂进 VT 状态机 —— 这就是整条数据流的全部,
    /// 中间没有 channel、没有缓冲窗口、没有序列化。
    pub fn spawn<F>(spec: PtySpawn, mut on_output: F) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty 失败")?;

        let mut cmd = CommandBuilder::new(&spec.program);
        for arg in &spec.args {
            cmd.arg(arg);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn `{}` 失败", spec.program))?;
        // slave 必须在 spawn 后立刻丢弃,否则子进程退出时 master 侧读不到 EOF。
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone reader 失败")?;
        let writer = pair.master.take_writer().context("take writer 失败")?;

        std::thread::spawn(move || {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => on_output(&buf[..n]),
                }
            }
        });

        Ok(Self {
            master: pair.master,
            child,
            writer: Arc::new(Mutex::new(writer)),
        })
    }

    /// 往 PTY 写字节(用户键入、粘贴、拖入的文件路径都走这里)。
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock();
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize 失败")
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("kill 失败")
    }

    /// 非阻塞地看一眼子进程是否已退出。
    pub fn try_wait(&mut self) -> Result<Option<u32>> {
        Ok(self.child.try_wait()?.map(|s| s.exit_code()))
    }
}

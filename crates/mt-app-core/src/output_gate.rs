//! 终端输出闸:PTY reader 线程 / 内置 SSH 会话的输出线程把字节推进 emulator 之前先过这一道。
//!
//! # 为什么要有它
//!
//! 一个 pane 的输出来源不止一个 —— 本地 PTY、内置 SSH 会话(远程项目 pane 的基底),以及右键
//! 「SSH 连接」接管时叠在基底上的那条会话 —— 同一时刻只许一个往 emulator 里写:
//!
//! - **接管期间**基底(本地 PTY 不杀)的输出整段丢弃:不进 emulator、不进 AI 感知、不进 git
//!   观察([`GateState::Drop`]);接管结束再放行([`GateState::Pass`])。丢而不是攒:接管可能
//!   持续几小时,本地 shell 这期间的输出(多半是 resize 引起的重画)回来时也用不上 —— 屏幕
//!   由接管前拍的快照还原;
//! - **SSH 会话刚建好**时,建连期间的提示(主机密钥 / banner / 密码提示的回显)还排在主线程上
//!   没画完,远端的第一批输出要等它们画完再进([`GateState::Hold`]):输出线程原地等,
//!   背压经 mt-ssh 的有界队列一路传回服务器,一个字节都不丢。
//!
//! # 同步口径
//!
//! 放行时整段旁路(推进 emulator + 各旁路 + 发唤醒信号)**在闸锁里跑**:主线程
//! [`OutputGate::set`] 返回时,不会还有一批按旧状态放进来的字节在途 —— 「先关闸、再
//! `begin_takeover`」「先 `end_takeover`、再开闸」靠的就是这一条。
//!
//! 锁序:闸 → emulator 的 term 锁。主线程改闸时**不许**持着 term 锁(否则与在途的推进互等)。
//!
//! ⚠️ 本地 PTY 的 reader 只用 `Pass` / `Drop`,**不用 `Hold`**:ConPTY 那头的管道被堵住,conhost
//! 会跟着卡住本地 shell 的写。`Hold` 的闸在主人消失前必须改成 `Pass` 或 `Drop`(宿主在 Drop 里
//! 兜一次),否则输出线程永远等在这里。

use parking_lot::{Condvar, Mutex};

/// 闸的三态,见模块注释。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateState {
    /// 输出线程原地等,直到状态变了。
    Hold,
    /// 放行。
    Pass,
    /// 丢弃。
    Drop,
}

/// 一道输出闸。两头(输出线程 / 主线程)各持一份 `Arc`。
pub struct OutputGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl std::fmt::Debug for OutputGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OutputGate").field(&self.state()).finish()
    }
}

impl OutputGate {
    pub fn new(state: GateState) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }

    pub fn state(&self) -> GateState {
        *self.state.lock()
    }

    /// 输出线程调用:`Hold` 时等到状态变了;`Pass` 时**在闸锁里**跑 `deliver` 并返回 `true`;
    /// `Drop` 时什么都不做,返回 `false`。
    pub fn run(&self, deliver: impl FnOnce()) -> bool {
        let mut state = self.state.lock();
        while *state == GateState::Hold {
            self.changed.wait(&mut state);
        }
        if *state == GateState::Drop {
            return false;
        }
        deliver();
        true
    }

    /// 主线程调用:改状态并叫醒在等的输出线程。返回时没有在途的 `deliver`(它在闸锁里跑)。
    pub fn set(&self, next: GateState) {
        let mut state = self.state.lock();
        *state = next;
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn 放行时跑_丢弃时不跑() {
        let gate = OutputGate::new(GateState::Pass);
        let runs = AtomicUsize::new(0);
        assert!(gate.run(|| {
            runs.fetch_add(1, Ordering::SeqCst);
        }));
        gate.set(GateState::Drop);
        assert!(!gate.run(|| {
            runs.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(gate.state(), GateState::Drop);
    }

    /// `Hold`:输出线程原地等;放行后才跑,而且是按放行那一刻的状态跑。
    #[test]
    fn 暂扣时等到放行再跑() {
        let gate = Arc::new(OutputGate::new(GateState::Hold));
        let runs = Arc::new(AtomicUsize::new(0));
        let worker = {
            let (gate, runs) = (gate.clone(), runs.clone());
            std::thread::spawn(move || {
                gate.run(|| {
                    runs.fetch_add(1, Ordering::SeqCst);
                })
            })
        };
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "暂扣期间一个字节都不许进");
        assert!(!worker.is_finished());
        gate.set(GateState::Pass);
        assert!(worker.join().unwrap());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// `Hold` 之后改成 `Drop`:在等的那一批直接丢掉,线程照常返回(主人没了也不会卡死)。
    #[test]
    fn 暂扣后丢弃则放走线程且不跑() {
        let gate = Arc::new(OutputGate::new(GateState::Hold));
        let runs = Arc::new(AtomicUsize::new(0));
        let worker = {
            let (gate, runs) = (gate.clone(), runs.clone());
            std::thread::spawn(move || {
                gate.run(|| {
                    runs.fetch_add(1, Ordering::SeqCst);
                })
            })
        };
        std::thread::sleep(Duration::from_millis(30));
        gate.set(GateState::Drop);
        assert!(!worker.join().unwrap());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    /// `set` 要等在途的那一批推进完才返回:返回之后再也看不到旧状态下放进来的字节。
    #[test]
    fn 改闸等在途的那一批跑完() {
        let gate = Arc::new(OutputGate::new(GateState::Pass));
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let (gate, order) = (gate.clone(), order.clone());
            std::thread::spawn(move || {
                gate.run(|| {
                    order.lock().push("推进开始");
                    std::thread::sleep(Duration::from_millis(120));
                    order.lock().push("推进结束");
                });
            })
        };
        // 等输出线程拿到闸锁
        while order.lock().is_empty() {
            std::thread::yield_now();
        }
        gate.set(GateState::Drop);
        order.lock().push("关闸返回");
        worker.join().unwrap();
        assert_eq!(*order.lock(), vec!["推进开始", "推进结束", "关闸返回"]);
    }
}

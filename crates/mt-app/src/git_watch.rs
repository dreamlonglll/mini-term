//! Git 面板的自动刷新链路:**pty-output 全局输出旁路**。
//!
//! # 原版长什么样
//!
//! `GitChanges.tsx:134-145` 与 `GitHistoryContent.tsx:338-349` 各自 `listen('pty-output')`,
//! 拿到 `{ptyId, data}` 之后:
//!
//! ```text
//! if (isAiPty(payload.ptyId)) return;          // AI pane 的输出不嗅探
//! if (GIT_REFRESH_PATTERNS.some(re => re.test(payload.data))) debouncedRefresh();  // 500ms 去抖
//! ```
//!
//! GPUI 侧 reader 回调(`pane.rs`)此前**不外传字节**,Git 面板拿不到内容 ——
//! 这是规格 §4.9 记的「本批最大的结构性缺口」。三条备选路里选的是 **(a) 全局输出旁路**
//! (主会话拍板):语义与原版一字不差,代价是热路径上多一次判断。
//!
//! # 热路径上到底加了什么
//!
//! ```text
//! reader 线程 ──► observe_output(pty_id, bytes)
//!                   ├─ ENABLED 是 false(Git 面板没开着)→ 一次 relaxed 原子读后 return
//!                   └─ 否则 上锁 → isAiPty 过滤 → 把尾部若干字节塞进有界环形缓冲
//! 主线程节拍 ──► drain_hit() → 跑 5 条口径 → 命中则清空缓冲并返回 true
//! ```
//!
//! **reader 线程上不跑任何模式匹配**(规格 §4.9 的原话:刷屏时会拖垮吞吐),
//! 它只做「塞缓冲」这一件常数开销的事;5 条口径全部挪到主线程的节拍上跑。
//! Git 面板收起 / 切到 sessions 时 [`set_enabled`] 关掉旁路,常态下这条路
//! 只剩一次原子读。
//!
//! ⚠️ **本模块与后续 Y 批的 git 状态着色共用**:文件树的 git 着色同样要在
//! 「外部跑了 git 命令」之后刷新(`FileTree.tsx:670` 是同一份嗅探代码)。
//! 再加消费方时**不要**各自开一条旁路 —— 那样 reader 上就有 N 次拷贝了;
//! 应当扩 [`drain_hit`] 为多订阅者(每个订阅者一个 dirty 位,缓冲共用一份)。
//!
//! # 与原版的两处细微差别(都是往严格里走)
//!
//! 1. **跨 payload 的模式也认**:原版逐条 payload 做 `re.test`,`create mode` 恰好
//!    被切成两次读时就漏了;这里是滚动窗口,接缝处照样命中。
//! 2. **不同 pane 的输出共用一个窗口**:理论上 A pane 的 `create ` 接 B pane 的
//!    `mode ` 能凑出一次误命中。后果只是多刷一次列表,不值得为它按 pane 分桶。

use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

/// 滚动窗口容量。git 的完成行(`3 files changed, 12 insertions(+)`)不过百字节,
/// 8 KiB 足够覆盖「命令跑完那一屏」,又不至于让刷屏进程把内存拖大。
const RING_CAP: usize = 8 * 1024;

/// 去抖窗口。原版两个面板各一个 500ms 定时器(`GitChanges.tsx:128-132`)。
pub const DEBOUNCE_MS: u64 = 500;

/// 主线程扫描节拍。原版是事件驱动(来一条 payload 判一条),这里改成轮询 ——
/// 100ms 的粒度对「命令跑完刷一下列表」完全够用,而且把 5 条口径的开销
/// 从 reader 线程挪到了这里。
pub const POLL_MS: u64 = 100;

/// 旁路总闸。Git 面板可见时才打开 —— 关着的时候这条路只剩一次原子读。
static ENABLED: AtomicBool = AtomicBool::new(false);

struct Tap {
    /// `aiPtyIds`(`src/utils/terminalCache.ts:106`)的对应物。
    ai_panes: HashSet<u32>,
    ring: VecDeque<u8>,
    /// 上次 [`drain_hit`] 之后有没有新字节。没有就不必重扫。
    dirty: bool,
}

/// `HashSet::new` 不是 const fn(`RandomState` 要随机种子),所以走 `LazyLock`
/// 而不是裸 `static Mutex`。首次访问初始化一次,之后与裸 static 同价。
static TAP: LazyLock<Mutex<Tap>> = LazyLock::new(|| {
    Mutex::new(Tap {
        ai_panes: HashSet::new(),
        ring: VecDeque::new(),
        dirty: false,
    })
});

/// 开/关旁路。关的时候顺手清空窗口 —— 下次打开不该被上一轮的残留立刻触发。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if !on {
        let mut tap = TAP.lock();
        tap.ring.clear();
        tap.dirty = false;
    }
}

/// 标记某个 PTY 是不是 AI pane。对应 `App.tsx:284` 的
/// `markAiPty(ptyId, status === 'ai-working' || status === 'ai-idle')`。
pub fn set_ai_pane(pty_id: u32, is_ai: bool) {
    let mut tap = TAP.lock();
    if is_ai {
        tap.ai_panes.insert(pty_id);
    } else {
        tap.ai_panes.remove(&pty_id);
    }
}

/// PTY 没了(对应 `terminalCache.ts:546` 的 `aiPtyIds.delete(ptyId)`)。
pub fn forget_pane(pty_id: u32) {
    TAP.lock().ai_panes.remove(&pty_id);
}

/// reader 线程的旁路入口。**必须保持常数开销**,见模块注释。
pub fn observe_output(pty_id: u32, bytes: &[u8]) {
    if !ENABLED.load(Ordering::Relaxed) || bytes.is_empty() {
        return;
    }
    let mut tap = TAP.lock();
    if tap.ai_panes.contains(&pty_id) {
        return;
    }
    // 一次读进来的量可能远超窗口(cat 大文件),只留尾部
    let tail = &bytes[bytes.len().saturating_sub(RING_CAP)..];
    tap.ring.extend(tail.iter().copied());
    while tap.ring.len() > RING_CAP {
        tap.ring.pop_front();
    }
    tap.dirty = true;
}

/// 主线程节拍:扫一遍窗口。命中则**清空窗口**并返回 `true`
/// (不清空的话同一段文字每个节拍都会再命中一次)。
pub fn drain_hit() -> bool {
    if !ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    let mut tap = TAP.lock();
    if !tap.dirty {
        return false;
    }
    tap.dirty = false;
    let hit = {
        let window = tap.ring.make_contiguous();
        matches_git_refresh(window)
    };
    if hit {
        tap.ring.clear();
    }
    hit
}

/// 五条口径,逐条对应原版的 `GIT_REFRESH_PATTERNS`(`GitChanges.tsx:19-25`):
///
/// ```text
/// /create mode/  /Switched to/  /Already up to date/
/// /insertions?\(\+\)/  /deletions?\(-\)/
/// ```
///
/// 后两条的 `s?` 展开成两个字面量 —— mt-app 没有 regex 依赖,而这五条本来
/// 就没有真正的正则语法(`\(` `\+` 都是转义的字面量)。
pub fn matches_git_refresh(haystack: &[u8]) -> bool {
    const NEEDLES: [&[u8]; 7] = [
        b"create mode",
        b"Switched to",
        b"Already up to date",
        b"insertion(+)",
        b"insertions(+)",
        b"deletion(-)",
        b"deletions(-)",
    ];
    NEEDLES
        .iter()
        .any(|needle| contains_subslice(haystack, needle))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试之间共用同一份进程级 tap,每条用例开头先复位。
    fn reset() {
        set_enabled(false);
        let mut tap = TAP.lock();
        tap.ai_panes.clear();
        tap.ring.clear();
        tap.dirty = false;
    }

    /// 五条口径逐条命中;不相干的输出不命中。
    #[test]
    fn 刷新口径逐条命中() {
        for sample in [
            "create mode 100644 src/main.rs",
            "Switched to branch 'main'",
            "Already up to date.",
            " 1 file changed, 3 insertions(+)",
            " 1 file changed, 1 insertion(+)",
            " 2 files changed, 5 deletions(-)",
            " 2 files changed, 1 deletion(-)",
        ] {
            assert!(
                matches_git_refresh(sample.as_bytes()),
                "应当命中: {sample}"
            );
        }
        for sample in [
            "",
            "$ ls -la",
            "warning: LF will be replaced by CRLF",
            // 大小写敏感:原版正则没有 /i
            "switched to branch 'main'",
            // `insertions(+)` 的括号是必须的
            "3 insertions",
        ] {
            assert!(
                !matches_git_refresh(sample.as_bytes()),
                "不该命中: {sample:?}"
            );
        }
    }

    /// 旁路的**全部有状态**用例合成一条。
    ///
    /// `TAP` / `ENABLED` 是进程级的(reader 线程要够得着),而 `cargo test`
    /// 默认多线程跑 —— 拆成多条会互相踩状态。
    #[test]
    fn 旁路状态机() {
        reset();

        // ① 总闸关着时什么都不记(常态开销 = 一次原子读)
        observe_output(1, b"create mode 100644 a.txt");
        assert!(!drain_hit());
        assert!(TAP.lock().ring.is_empty());

        // ② AI pane 的输出被跳过(`isAiPty` 那道闸)
        set_enabled(true);
        set_ai_pane(7, true);
        observe_output(7, b"create mode 100644 a.txt");
        assert!(!drain_hit(), "AI pane 的输出不该触发刷新");

        // ③ 普通 pane 照常命中,且命中后窗口清空(同一段文字不会每拍再触发)
        observe_output(8, b"create mode 100644 a.txt");
        assert!(drain_hit());
        assert!(!drain_hit(), "没有新字节就不该重扫");

        // ④ 取消 AI 标记之后同一个 pane 又开始被嗅探
        set_ai_pane(7, false);
        observe_output(7, b" 1 file changed, 2 insertions(+)");
        assert!(drain_hit());

        // ⑤ 跨两次读切开的模式照样命中(原版逐 payload 判定会漏)
        observe_output(1, b"...create mo");
        assert!(!drain_hit());
        observe_output(1, b"de 100644 a.txt\n");
        assert!(drain_hit());

        // ⑥ 环形缓冲有界:刷屏不会把内存吃掉,窗口只留尾部
        let flood = vec![b'x'; RING_CAP * 3];
        observe_output(1, &flood);
        assert_eq!(TAP.lock().ring.len(), RING_CAP);
        assert!(!drain_hit());
        observe_output(1, b"Switched to branch 'x'");
        assert!(drain_hit());

        // ⑦ 关闸清空残留 —— 下次打开不该被上一轮的内容立刻触发
        observe_output(1, b"create mode 100644 a.txt");
        set_enabled(false);
        set_enabled(true);
        assert!(!drain_hit());

        // ⑧ pane 关掉后摘标记,否则新 PTY 复用同一个 id 会被误当成 AI pane
        set_ai_pane(3, true);
        forget_pane(3);
        observe_output(3, b"Already up to date.");
        assert!(drain_hit());

        reset();
    }
}

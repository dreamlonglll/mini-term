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
//! # 多订阅者(Y 批扩)
//!
//! 文件树的 git 状态着色要在「外部跑了 git 命令」之后刷新(`FileTree.tsx:667-674`
//! 与 `GitChanges.tsx:134-145` 是同一份嗅探代码),于是这条旁路现在有两个消费方
//! (见 [`Subscriber`])。**没有另开第二条旁路**:reader 线程上仍然只有一次拷贝,
//! 缓冲共用一份,逐订阅者的只有一个读游标。
//!
//! 原注释设想的是「每人一个 dirty 位」,落地时换成了**游标**,因为光有 dirty 位
//! 过不去这一关:命中之后要清空缓冲(否则同一段文字每个节拍都会再命中一次),
//! 而缓冲是共享的 —— A 命中清了缓冲,还没轮到的 B 就扫了个空,文件树静默漏刷。
//! 游标版本里「已经扫到哪」是逐订阅者的,A 清不掉 B 的那一份。
//!
//! ```text
//! ring:  … create mode 100644 a.txt …
//!        ↑head_seq                   ↑total = head_seq + ring.len()
//!              ↑GitPanel.cursor   ↑FileTree.cursor
//! ```
//!
//! 每次 [`drain_hit_for`] 从 `cursor - OVERLAP` 扫到 `total`(接缝处被切成两半的
//! 模式照样拼得回来),扫完把 `cursor` 推到 `total`;**命中的那一次不留接缝** ——
//! 否则下一拍会把同一段文字再认一遍。
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

/// 扫描接缝:两次 drain 之间要多回看这么多字节,免得跨拍被切成两半的模式漏掉。
/// 取「最长口径 - 1」= `Already up to date`(18 字节)- 1。
const OVERLAP: u64 = 17;

/// 旁路的消费方。加一个消费方 = 在这里加一个 variant + 把 [`SUB_COUNT`] 加一,
/// **不是**再抄一条旁路出来(见模块注释)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subscriber {
    /// 右抽屉的 Git 面板(变更列表 / 提交历史)。
    GitPanel,
    /// 中栏文件树的 git 状态着色。
    FileTree,
}

/// 订阅者数量。数组下标即 [`Subscriber`] 的声明序。
const SUB_COUNT: usize = 2;

impl Subscriber {
    fn idx(self) -> usize {
        match self {
            Subscriber::GitPanel => 0,
            Subscriber::FileTree => 1,
        }
    }
}

/// 旁路总闸:**任一**订阅者开着就为真。reader 线程只看这一个原子量,
/// 全关时那条路仍然只剩一次 relaxed 读。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 逐订阅者的读状态。
#[derive(Clone, Copy)]
struct Sub {
    enabled: bool,
    /// 已经扫到的序号(不含);`cursor >= total` 就是「没有新字节」= 原来的 dirty 位。
    cursor: u64,
    /// 上一次是命中收的场:这一次从 `cursor` 起扫,不留接缝(否则重复命中)。
    skip_overlap: bool,
}

struct Tap {
    /// `aiPtyIds`(`src/utils/terminalCache.ts:106`)的对应物。
    ai_panes: HashSet<u32>,
    ring: VecDeque<u8>,
    /// `ring` 首字节的全局序号。滚掉多少就加多少,单调不回头。
    head_seq: u64,
    subs: [Sub; SUB_COUNT],
}

impl Tap {
    fn total_seq(&self) -> u64 {
        self.head_seq + self.ring.len() as u64
    }
}

/// `HashSet::new` 不是 const fn(`RandomState` 要随机种子),所以走 `LazyLock`
/// 而不是裸 `static Mutex`。首次访问初始化一次,之后与裸 static 同价。
static TAP: LazyLock<Mutex<Tap>> = LazyLock::new(|| {
    Mutex::new(Tap {
        ai_panes: HashSet::new(),
        ring: VecDeque::new(),
        head_seq: 0,
        subs: [Sub {
            enabled: false,
            cursor: 0,
            skip_overlap: false,
        }; SUB_COUNT],
    })
});

/// 开/关某个订阅者。
///
/// - **开**:游标直接推到窗口末尾 —— 打开的那一刻不该被上一轮的残留立刻触发
///   (原版每次挂 listener 也是从此刻起才收 payload);
/// - **关**:全关之后清空窗口,免得白留着 8 KiB。
pub fn set_enabled_for(sub: Subscriber, on: bool) {
    let mut tap = TAP.lock();
    let total = tap.total_seq();
    let slot = &mut tap.subs[sub.idx()];
    slot.enabled = on;
    slot.cursor = total;
    slot.skip_overlap = true;
    let any = tap.subs.iter().any(|s| s.enabled);
    if !any {
        tap.head_seq = total;
        tap.ring.clear();
    }
    ENABLED.store(any, Ordering::Relaxed);
}

/// 老签名 = [`Subscriber::GitPanel`] 的快捷方式(V 批的调用点原样保留)。
pub fn set_enabled(on: bool) {
    set_enabled_for(Subscriber::GitPanel, on);
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
        tap.head_seq += 1;
    }
}

/// 主线程节拍:替**某一个订阅者**扫一遍它还没看过的那段窗口。
///
/// 命中后把游标推到末尾并抹掉接缝(不抹的话同一段文字下一拍会再命中一次);
/// 窗口本身**不清空** —— 那是共享的,清了别的订阅者就扫了个空。
pub fn drain_hit_for(sub: Subscriber) -> bool {
    if !ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    let mut tap = TAP.lock();
    let total = tap.total_seq();
    let head = tap.head_seq;
    let slot = tap.subs[sub.idx()];
    if !slot.enabled || slot.cursor >= total {
        return false;
    }
    // 从上次扫到的地方往回退一个接缝,再与窗口首端取交(滚掉的部分找不回来了)
    let from = if slot.skip_overlap {
        slot.cursor
    } else {
        slot.cursor.saturating_sub(OVERLAP)
    }
    .max(head);
    let offset = (from - head) as usize;
    let hit = {
        let window = tap.ring.make_contiguous();
        matches_git_refresh(&window[offset.min(window.len())..])
    };
    let slot = &mut tap.subs[sub.idx()];
    slot.cursor = total;
    slot.skip_overlap = hit;
    hit
}

/// 老签名 = [`Subscriber::GitPanel`] 的快捷方式(V 批的调用点原样保留)。
pub fn drain_hit() -> bool {
    drain_hit_for(Subscriber::GitPanel)
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
        set_enabled_for(Subscriber::GitPanel, false);
        set_enabled_for(Subscriber::FileTree, false);
        let mut tap = TAP.lock();
        tap.ai_panes.clear();
        tap.ring.clear();
        tap.head_seq = 0;
        tap.subs = [Sub {
            enabled: false,
            cursor: 0,
            skip_overlap: false,
        }; SUB_COUNT];
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

        // ─── 以下是 Y 批扩多订阅者补的用例 ───────────────────
        // 同样只能挂在这一条测试里:`TAP` / `ENABLED` 是进程级的,
        // 单开一个 `#[test]` 会与上面这段并行互踩(实测必炸)。

        // ⑨ 两家各收一份,**谁先扫都不会把另一家饿死**。这是本次扩展的核心
        //    不变量 —— 旧实现命中即清空共享窗口,换成两个消费方之后就是
        //    「Git 面板刷了、文件树静默漏刷」
        reset();
        set_enabled_for(Subscriber::GitPanel, true);
        set_enabled_for(Subscriber::FileTree, true);

        observe_output(1, b" 2 files changed, 5 deletions(-)");
        assert!(drain_hit_for(Subscriber::GitPanel));
        assert!(
            drain_hit_for(Subscriber::FileTree),
            "先扫的那家不许把窗口清空"
        );
        // 各自都只认一次
        assert!(!drain_hit_for(Subscriber::GitPanel));
        assert!(!drain_hit_for(Subscriber::FileTree));

        // 只开一家时另一家一律不响(reader 侧总闸仍然开着)
        set_enabled_for(Subscriber::GitPanel, false);
        observe_output(1, b"Switched to branch 'y'");
        assert!(!drain_hit_for(Subscriber::GitPanel));
        assert!(drain_hit_for(Subscriber::FileTree));

        // 后开的那家不吃开闸之前的存货(与原版「此刻起才挂 listener」同口径)
        set_enabled_for(Subscriber::GitPanel, true);
        assert!(!drain_hit_for(Subscriber::GitPanel));

        // 两家全关 = reader 侧总闸也关掉
        set_enabled_for(Subscriber::FileTree, false);
        set_enabled_for(Subscriber::GitPanel, false);
        observe_output(1, b"create mode 100644 a.txt");
        assert!(TAP.lock().ring.is_empty());

        // ⑩ 命中之后不留接缝:同一段文字不会因为回看窗口被认第二次
        reset();
        set_enabled_for(Subscriber::FileTree, true);
        observe_output(1, b"Already up to date.");
        assert!(drain_hit_for(Subscriber::FileTree));
        // 紧接着来一段无关输出:回看接缝里还压着上一段,但命中过的不再算数
        observe_output(1, b"$ ls");
        assert!(!drain_hit_for(Subscriber::FileTree));

        reset();
    }
}

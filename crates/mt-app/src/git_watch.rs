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
//!                                → 给登记了唤醒口、这一轮还没叫过的订阅者丢一声
//! 主线程 ──► drain_hit() → 跑 5 条口径 → 命中则清空缓冲并返回 true
//! ```
//!
//! **reader 线程上不跑任何模式匹配**(规格 §4.9 的原话:刷屏时会拖垮吞吐),
//! 它只做「塞缓冲」这一件常数开销的事;5 条口径全部挪到主线程上跑。
//! Git 面板收起 / 切到 sessions 时 [`set_enabled`] 关掉旁路,常态下这条路
//! 只剩一次原子读。
//!
//! # 唤醒:谁来叫主线程扫
//!
//! 两个订阅者的扫法不一样:
//!
//! - **Git 面板**:面板开着时 100ms 节拍轮询(收起即停,见 `git_panel.rs`);
//! - **文件树**:常驻面板,再挂一个 100ms 常驻节拍就是「无项目 / 窗口最小化时
//!   主线程每秒也被空唤醒 10 次」。它改为**按需唤醒**:用 [`wake_channel_for`]
//!   登记一个唤醒口,reader 线程往窗口里塞了新字节就往口里丢一声。
//!
//! 叫醒**一轮只叫一次**:丢过一声之后置 `wake_sent`,直到该订阅者来
//! [`drain_hit_for`] 扫过才复位 —— 刷屏时 reader 一秒读几千次,口里也只躺着一声,
//! 主线程被叫醒的次数由消费方自己节流(文件树两次扫描之间至少隔 [`POLL_MS`])。
//! 置位与复位都在同一把锁里,「扫完之后又来的字节」一定能再叫醒一次,不会漏。
//!
//! 另有一条不经字节的路:[`request_refresh`] 点名让某个订阅者刷一次(Git 面板发现仓库
//! 变了 —— `.git` 监听命中、面板里的写操作做完 —— 叫文件树重算 git 标记),下一次扫描
//! 直接算命中。
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

use futures::channel::mpsc;
use parking_lot::Mutex;

/// 滚动窗口容量。git 的完成行(`3 files changed, 12 insertions(+)`)不过百字节,
/// 8 KiB 足够覆盖「命令跑完那一屏」,又不至于让刷屏进程把内存拖大。
const RING_CAP: usize = 8 * 1024;

/// 去抖窗口。原版两个面板各一个 500ms 定时器(`GitChanges.tsx:128-132`)。
pub const DEBOUNCE_MS: u64 = 500;

/// 主线程扫描间隔。原版是事件驱动(来一条 payload 判一条),这里按批扫 ——
/// 100ms 的粒度对「命令跑完刷一下列表」完全够用,而且把 5 条口径的开销
/// 从 reader 线程挪到了主线程。Git 面板拿它当轮询节拍,文件树拿它当两次
/// 扫描之间的最小间隔(节流,见模块注释「唤醒」一节)。
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
    /// 已经往唤醒口丢过一声、该订阅者还没来扫。期间再来字节不重复叫醒。
    wake_sent: bool,
    /// 有人点名要它刷一次([`request_refresh`]):下一次扫描不看字节、直接算命中。
    forced: bool,
}

const IDLE_SUB: Sub = Sub {
    enabled: false,
    cursor: 0,
    skip_overlap: false,
    wake_sent: false,
    forced: false,
};

struct Tap {
    /// `aiPtyIds`(`src/utils/terminalCache.ts:106`)的对应物。
    ai_panes: HashSet<u32>,
    ring: VecDeque<u8>,
    /// `ring` 首字节的全局序号。滚掉多少就加多少,单调不回头。
    head_seq: u64,
    subs: [Sub; SUB_COUNT],
    /// 逐订阅者的唤醒口(见 [`wake_channel_for`])。`None` = 该订阅者自己轮询。
    wakers: [Option<mpsc::UnboundedSender<()>>; SUB_COUNT],
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
        subs: [IDLE_SUB; SUB_COUNT],
        wakers: [const { None }; SUB_COUNT],
    })
});

/// 给某个订阅者登记唤醒口,返回接收端:此后该订阅者**开着**时,窗口里每进一批
/// 新字节(AI pane 的不算)就往口里丢一声,一轮只丢一次(见模块注释「唤醒」)。
///
/// 收到一声 ≠ 命中,只是「有新字节,值得扫一遍」—— 该订阅者照旧调
/// [`drain_hit_for`] 判定,扫过之后才会被再次叫醒。重复登记 = 换新口,
/// 旧的接收端从此收不到。
pub fn wake_channel_for(sub: Subscriber) -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded();
    let mut tap = TAP.lock();
    tap.wakers[sub.idx()] = Some(tx);
    tap.subs[sub.idx()].wake_sent = false;
    rx
}

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
    slot.wake_sent = false;
    slot.forced = false;
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

/// 不经输出旁路,直接点名让某个订阅者刷一次:下一次 [`drain_hit_for`] 算命中,
/// 登记了唤醒口的(文件树)当场叫醒。
///
/// Git 面板知道仓库变了而终端里不一定有输出的时候用 —— `.git` 监听命中(AI 在 pane 里
/// 提交、合并 / 变基「继续」做完)、面板里的暂存 / 丢弃 / 提交做完:文件树的 git 标记
/// 要跟着重算,否则 rebase 已经干净了文件树还标着 M。订阅者关着时什么都不做
/// (远程项目 / 没项目本来就不拉 git 状态)。刷新仍走订阅者自己的去抖。
pub fn request_refresh(sub: Subscriber) {
    let mut tap = TAP.lock();
    let tap = &mut *tap;
    let slot = &mut tap.subs[sub.idx()];
    if !slot.enabled {
        return;
    }
    slot.forced = true;
    if !slot.wake_sent
        && let Some(waker) = &tap.wakers[sub.idx()]
    {
        slot.wake_sent = true;
        let _ = waker.unbounded_send(());
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
    let tap = &mut *tap;
    tap.head_seq += push_bounded(&mut tap.ring, bytes, RING_CAP);
    // 叫醒:开着、登记了唤醒口、这一轮还没叫过的订阅者各丢一声。
    // `unbounded_send` 不阻塞;接收端没了(订阅者已销毁)就当没这回事
    for (slot, waker) in tap.subs.iter_mut().zip(&tap.wakers) {
        if slot.enabled
            && !slot.wake_sent
            && let Some(waker) = waker
        {
            slot.wake_sent = true;
            let _ = waker.unbounded_send(());
        }
    }
}

/// 把 `bytes` 追加进容量为 `cap` 的滚动窗口,返回**从窗口头部滚掉**的字节数
/// (调用方据此推进 `head_seq`)。
///
/// 一次读进来的量可能远超窗口(cat 大文件),只留尾部;尾部本身不超过 `cap`,
/// 所以要滚掉的一定全是窗口里的旧字节 —— 先整段 `drain` 腾位置再追加,
/// 不逐字节 `pop_front`,窗口也不会临时涨到两倍容量。
fn push_bounded(ring: &mut VecDeque<u8>, bytes: &[u8], cap: usize) -> u64 {
    let tail = &bytes[bytes.len().saturating_sub(cap)..];
    let overflow = (ring.len() + tail.len())
        .saturating_sub(cap)
        .min(ring.len());
    ring.drain(..overflow);
    ring.extend(tail);
    overflow as u64
}

/// 替**某一个订阅者**扫一遍它还没看过的那段窗口(Git 面板在节拍上调,
/// 文件树在被唤醒口叫醒后调)。
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
    // 丢过的那一声就算领走了:从这一刻起再进来的字节要重新叫醒它
    tap.subs[sub.idx()].wake_sent = false;
    let slot = tap.subs[sub.idx()];
    if !slot.enabled {
        return false;
    }
    // 被点名刷新(见 `request_refresh`):不看字节,游标推到末尾、不留接缝
    if slot.forced {
        let slot = &mut tap.subs[sub.idx()];
        slot.forced = false;
        slot.cursor = total;
        slot.skip_overlap = true;
        return true;
    }
    if slot.cursor >= total {
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
        tap.subs = [IDLE_SUB; SUB_COUNT];
        tap.wakers = [const { None }; SUB_COUNT];
    }

    /// 旧实现(逐字节 `pop_front`)原样留作对照。
    fn push_bounded_by_pop(ring: &mut VecDeque<u8>, bytes: &[u8], cap: usize) -> u64 {
        let tail = &bytes[bytes.len().saturating_sub(cap)..];
        ring.extend(tail.iter().copied());
        let mut dropped = 0;
        while ring.len() > cap {
            ring.pop_front();
            dropped += 1;
        }
        dropped
    }

    /// 批量出队与逐字节出队逐步等价:窗口内容、滚掉的字节数(`head_seq` 的增量)
    /// 两样都要一致,且窗口任何时刻都不超过容量。纯函数,不碰进程级 `TAP`。
    #[test]
    fn 批量出队与逐字节出队等价() {
        const CAP: usize = 8;
        let writes: [&[u8]; 9] = [
            b"",
            b"abc",
            b"defgh",       // 恰好填满
            b"i",           // 溢出 1
            b"jklmnopq",    // 恰好一整窗
            b"rstuvwxyz01", // 单次就超过容量:只留尾部
            b"",
            b"23",
            b"456789ABCDEFGHIJKLMNOP",
        ];
        let mut batch = VecDeque::new();
        let mut naive = VecDeque::new();
        let (mut batch_head, mut naive_head) = (0u64, 0u64);
        for bytes in writes {
            batch_head += push_bounded(&mut batch, bytes, CAP);
            naive_head += push_bounded_by_pop(&mut naive, bytes, CAP);
            assert_eq!(batch, naive, "写入 {:?} 后窗口内容不一致", bytes);
            assert_eq!(
                batch_head, naive_head,
                "写入 {:?} 后 head_seq 不一致",
                bytes
            );
            assert!(batch.len() <= CAP);
        }
        assert_eq!(batch.iter().copied().collect::<Vec<u8>>(), b"IJKLMNOP");
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

        // ─── 按需唤醒(文件树不再挂 100ms 常驻节拍) ───────────
        // 同上,只能挂在这一条里。
        reset();
        let mut wake = wake_channel_for(Subscriber::FileTree);
        let pending = |rx: &mut mpsc::UnboundedReceiver<()>| {
            let mut n = 0;
            while rx.try_recv().is_ok() {
                n += 1;
            }
            n
        };

        // ⑪ 订阅者没开:有字节也不叫(Git 面板开着让总闸通电,文件树关着)
        set_enabled_for(Subscriber::GitPanel, true);
        observe_output(1, b"create mode 100644 a.txt");
        assert_eq!(pending(&mut wake), 0, "没开的订阅者不该被叫醒");

        // ⑫ 开着:第一批字节叫一声,扫之前再来多少批都不重复叫
        set_enabled_for(Subscriber::FileTree, true);
        observe_output(1, b"$ git st");
        observe_output(1, b"atus\n");
        observe_output(1, b"On branch main\n");
        assert_eq!(pending(&mut wake), 1, "一轮只叫一次");

        // ⑬ 扫过(不管命没命中)才复位,之后的新字节再叫一次
        assert!(!drain_hit_for(Subscriber::FileTree));
        assert_eq!(pending(&mut wake), 0, "扫描本身不叫醒");
        observe_output(1, b" 1 file changed, 1 insertion(+)");
        assert_eq!(pending(&mut wake), 1);
        assert!(drain_hit_for(Subscriber::FileTree));

        // ⑭ AI pane 的输出不进窗口,也就不叫醒
        set_ai_pane(9, true);
        observe_output(9, b"create mode 100644 b.txt");
        assert_eq!(pending(&mut wake), 0, "AI pane 刷屏不该叫醒文件树");

        // ⑮ 没登记唤醒口的订阅者(Git 面板自己轮询)不受影响,照旧扫得到
        observe_output(1, b"Switched to branch 'z'");
        assert_eq!(pending(&mut wake), 1);
        assert!(drain_hit_for(Subscriber::GitPanel));
        assert!(drain_hit_for(Subscriber::FileTree));

        // ⑯ 关掉再开:复位叫醒标记,开闸后的第一批字节照常叫
        observe_output(1, b"x");
        set_enabled_for(Subscriber::FileTree, false);
        set_enabled_for(Subscriber::FileTree, true);
        let _ = pending(&mut wake);
        observe_output(1, b"y");
        assert_eq!(pending(&mut wake), 1, "重开之后不许卡在「叫过了」");

        // ⑰ 重新登记 = 换新口:旧接收端收不到,新接收端接得上
        let mut fresh = wake_channel_for(Subscriber::FileTree);
        observe_output(1, b"z");
        assert_eq!(pending(&mut wake), 0);
        assert_eq!(pending(&mut fresh), 1);

        // ─── 点名刷新(Git 面板叫文件树重算标记) ───────────────
        // ⑱ 不带字节也叫醒、扫一次算命中、只算一次
        assert!(!drain_hit_for(Subscriber::FileTree));
        let _ = pending(&mut fresh);
        request_refresh(Subscriber::FileTree);
        assert_eq!(pending(&mut fresh), 1, "点名刷新要叫醒订阅者");
        assert!(drain_hit_for(Subscriber::FileTree));
        assert!(!drain_hit_for(Subscriber::FileTree), "点名一次只算一次");

        // ⑲ 订阅者关着时点名无效,开闸也不带出旧的点名
        set_enabled_for(Subscriber::FileTree, false);
        request_refresh(Subscriber::FileTree);
        set_enabled_for(Subscriber::FileTree, true);
        assert!(!drain_hit_for(Subscriber::FileTree));

        reset();
    }
}

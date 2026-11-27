/**
 * 对话镜像消息列表的合并规则(纯函数,零运行时依赖,`tests/mobilePureLogic.test.cjs` 直接引用)。
 *
 * 背景:桌面端一次镜像「绑定」内 seq 从 0 连续递增(`crates/mt-relay/src/mirror.rs` 的
 * `history_slice` 依赖 `messages[i].seq == i`);订阅 / 重订阅 / 换绑时桌面端回发的
 * `mirrorSnapshot` 永远是**最近一页**(尾部),更早的要靠 `mirrorHistory` 分页拉。
 * 换绑(`/clear` 开新会话、记录被整份重写)后 seq 从 0 重新编号,同一个 seq 可能是另一条消息。
 *
 * 重连后同一会话会重订阅一次,快照整体替换会把用户「加载更早」拿到的历史丢掉,
 * 所以快照先尝试与已有消息按 seq 合并,判不准是不是同一条时间线就退回整体替换。
 */
import type { MirrorMessage } from '../protocol';

export interface SnapshotMergeResult {
  messages: MirrorMessage[];
  hasMore: boolean;
  /** true = 保留了快照之前已加载的更早消息;false = 按快照整体替换 */
  merged: boolean;
}

/** 按 seq 升序、同 seq 只留最后一条(桌面端本就有序不重,这里只是兜底)。 */
function normalize(messages: MirrorMessage[]): MirrorMessage[] {
  let sorted = true;
  for (let i = 1; i < messages.length; i += 1) {
    if (messages[i].seq <= messages[i - 1].seq) {
      sorted = false;
      break;
    }
  }
  if (sorted) return messages;
  const bySeq = new Map<number, MirrorMessage>();
  for (const m of messages) bySeq.set(m.seq, m);
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

/**
 * 两条同 seq 的消息是不是「同一条」。不比 `source`:重订阅时桌面端会对整份历史重新
 * 改标来源(移动端指令 / 桌面输入),同一条消息的来源可能被纠正;正文、时间戳、
 * 种类与提问身份才是消息本身。
 */
function sameMessage(a: MirrorMessage, b: MirrorMessage): boolean {
  return (
    a.seq === b.seq &&
    a.content === b.content &&
    a.timestamp === b.timestamp &&
    (a.kind ?? '') === (b.kind ?? '') &&
    (a.questionId ?? '') === (b.questionId ?? '') &&
    (a.refSeq ?? -1) === (b.refSeq ?? -1) &&
    (a.toolName ?? '') === (b.toolName ?? '') &&
    (a.toolTarget ?? '') === (b.toolTarget ?? '')
  );
}

/** 字段逐一相同:可以沿用旧对象引用(省掉一次行重建,滚动锚点也认得出)。 */
function identical(a: MirrorMessage, b: MirrorMessage): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * 把新到的快照并进已有消息。
 *
 * 合并规则(全部满足才合并,否则按快照整体替换):
 * 1. 已有与快照都非空;
 * 2. 已有的最大 seq 不超过快照的最大 seq —— 快照是最新的尾页,旧列表里有比它还新的
 *    消息,说明不是同一条时间线(换绑 / 记录被重写变短);
 * 3. 两者在 seq 上**有重叠**,且重叠区间里同 seq 的消息逐条是同一条(见 `sameMessage`)——
 *    没有重叠(断线期间新增超过一页,中间有缺口;或者恰好首尾相接)就无从核对,按替换处理,
 *    宁可丢已加载的历史也不把两段会话拼在一起。
 *
 * 合并结果 = 已有里 seq < 快照首条 seq 的那一截(更早、快照不含的部分,原样保留)
 *          + 整份快照(快照覆盖它所含的区间;与旧消息逐字段相同的沿用旧对象)。
 * hasMore:保留了更早的部分时沿用旧值(它描述的是已有最早一条之前还有没有),否则取快照的。
 */
export function mergeMirrorSnapshot(
  prevMessages: MirrorMessage[],
  prevHasMore: boolean,
  snapshotMessages: MirrorMessage[],
  snapshotHasMore: boolean,
): SnapshotMergeResult {
  const snap = normalize(snapshotMessages);
  const replace: SnapshotMergeResult = { messages: snap, hasMore: snapshotHasMore, merged: false };
  const prev = normalize(prevMessages);
  if (prev.length === 0 || snap.length === 0) return replace;

  const snapMin = snap[0].seq;
  const snapMax = snap[snap.length - 1].seq;
  const prevMax = prev[prev.length - 1].seq;
  if (prevMax > snapMax || prevMax < snapMin) return replace;

  const snapBySeq = new Map<number, MirrorMessage>();
  for (const m of snap) snapBySeq.set(m.seq, m);
  const prevBySeq = new Map<number, MirrorMessage>();
  const older: MirrorMessage[] = [];
  for (const m of prev) {
    if (m.seq < snapMin) {
      older.push(m);
      continue;
    }
    const counterpart = snapBySeq.get(m.seq);
    if (!counterpart || !sameMessage(m, counterpart)) return replace;
    prevBySeq.set(m.seq, m);
  }

  const tail = snap.map((m) => {
    const old = prevBySeq.get(m.seq);
    return old && identical(old, m) ? old : m;
  });
  if (older.length === 0) {
    return { messages: tail, hasMore: snapshotHasMore, merged: false };
  }
  return { messages: [...older, ...tail], hasMore: prevHasMore, merged: true };
}

/**
 * 实时追加:只收 seq 比已有最后一条大的(同一连接上桌面端按序推送,换绑必先发快照,
 * 不会出现回退的 seq;真出现了就是重复,丢掉)。没有新消息时返回原数组(引用不变 = 不重渲染)。
 */
export function appendMirrorMessages(
  existing: MirrorMessage[],
  incoming: MirrorMessage[],
): MirrorMessage[] {
  const last = existing.length > 0 ? existing[existing.length - 1].seq : -1;
  const fresh = normalize(incoming).filter((m) => m.seq > last);
  return fresh.length > 0 ? [...existing, ...fresh] : existing;
}

/** 分页加载更早:只收 seq 比已有第一条小的,接在头部。 */
export function prependOlderMirrorMessages(
  existing: MirrorMessage[],
  older: MirrorMessage[],
): MirrorMessage[] {
  const first = existing.length > 0 ? existing[0].seq : Number.POSITIVE_INFINITY;
  const fresh = normalize(older).filter((m) => m.seq < first);
  return fresh.length > 0 ? [...fresh, ...existing] : existing;
}

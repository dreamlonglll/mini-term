/**
 * 镜像消息数组 → 渲染行(文本 / 打断提示 / 提问卡片 / 工具组)。
 *
 * 纯函数,不碰 React:分组规则单独成文件,好对着协议核对,也不和渲染细节搅在一起。
 *
 * 引用稳定是这里的第二个职责。镜像每秒增量推一次,`relay.ts` 追加时旧消息对象
 * 原样留着,行组件都是 `React.memo`——只要行对象(或它拆出去的 props)引用不变,
 * 没变的行就跳过渲染,长对话里那几百段 Markdown 不必每秒重新解析一遍。
 * 所以每次重建都拿上一轮的行做结构共享:同 key 且内容逐项同引用的行原样复用。
 */
import type { MirrorMessage } from './protocol';

export interface TextRow {
  kind: 'message';
  key: number;
  msg: MirrorMessage;
}

/** Claude 写进会话记录的打断标记:不当输入气泡,渲染成居中的小提示 */
export interface InterruptRow {
  kind: 'interrupt';
  key: number;
  msg: MirrorMessage;
}

export interface QuestionRow {
  kind: 'question';
  key: number;
  msg: MirrorMessage;
  /** 仍可作答:其后没有任何非工具消息,且未见作答标记 */
  active: boolean;
  /** 回流的 questionAnswered 标记;null = 还没有 */
  marker: MirrorMessage | null;
}

/** 连续的 kind="tool" 消息折叠成的一组;key 取组内首条 seq(展开状态按它存) */
export interface ToolsRow {
  kind: 'tools';
  key: number;
  tools: readonly MirrorMessage[];
}

export type MirrorRow = TextRow | InterruptRow | QuestionRow | ToolsRow;

const INTERRUPT_PREFIX = '[Request interrupted by user';

/**
 * 打断标记:Claude 在用户 Esc / Ctrl+C 打断时往会话记录里写一条「用户消息」,
 * 镜像照实转过来就成了一个写着英文原文的输入气泡,读起来像用户自己敲的。
 */
export function isInterruptMarker(m: MirrorMessage): boolean {
  return (
    (m.source === 'desktop' || m.source === 'mobile') &&
    m.content.trim().startsWith(INTERRUPT_PREFIX)
  );
}

function sameMessages(a: readonly MirrorMessage[], b: readonly MirrorMessage[]): boolean {
  return a.length === b.length && a.every((m, i) => m === b[i]);
}

function sameRow(a: MirrorRow, b: MirrorRow): boolean {
  switch (a.kind) {
    case 'tools':
      return b.kind === 'tools' && sameMessages(a.tools, b.tools);
    case 'question':
      return (
        b.kind === 'question' &&
        a.msg === b.msg &&
        a.active === b.active &&
        a.marker === b.marker
      );
    default:
      return b.kind === a.kind && 'msg' in b && b.msg === a.msg;
  }
}

/**
 * 把消息数组切成渲染行。`prev` 传上一轮的结果用于结构共享(首次传空数组)。
 *
 * - 连续的工具消息并成一组,不跨文本消息、提问卡片或作答标记——作答标记本身
 *   不成行,但它是回合里的一个节点,工具组照样在这里断开;
 * - 提问卡片的 active 判定:该提问之后没有任何**非工具**消息。同一回合里并行的
 *   工具调用可能排在提问后面,按「是不是最后一条消息」判会让卡片误失效;
 *   旧桌面端不发工具消息,两种判法结果一致。
 */
export function buildMirrorRows(
  messages: readonly MirrorMessage[],
  prev: readonly MirrorRow[],
): MirrorRow[] {
  // 作答标记按 refSeq 建索引:逐卡片 find 是 O(n²),长对话可感
  const markers = new Map<number, MirrorMessage>();
  let lastNonTool = -1;
  messages.forEach((m, i) => {
    if (m.kind === 'questionAnswered' && m.refSeq !== undefined) markers.set(m.refSeq, m);
    if (m.kind !== 'tool') lastNonTool = i;
  });

  const prevByKey = new Map<number, MirrorRow>();
  for (const r of prev) prevByKey.set(r.key, r);
  const share = (next: MirrorRow): MirrorRow => {
    const old = prevByKey.get(next.key);
    return old && sameRow(old, next) ? old : next;
  };

  const rows: MirrorRow[] = [];
  let groupStart = -1;
  const flushTools = (end: number) => {
    if (groupStart < 0) return;
    const tools = messages.slice(groupStart, end);
    rows.push(share({ kind: 'tools', key: tools[0].seq, tools }));
    groupStart = -1;
  };

  messages.forEach((m, i) => {
    if (m.kind === 'tool') {
      if (groupStart < 0) groupStart = i;
      return;
    }
    flushTools(i);
    if (m.kind === 'questionAnswered') return;
    if (m.kind === 'question') {
      const marker = markers.get(m.seq) ?? null;
      rows.push(
        share({ kind: 'question', key: m.seq, msg: m, active: i === lastNonTool && !marker, marker }),
      );
      return;
    }
    rows.push(share({ kind: isInterruptMarker(m) ? 'interrupt' : 'message', key: m.seq, msg: m }));
  });
  flushTools(messages.length);
  return rows;
}

/**
 * 当前有没有一张可作答的提问卡片。可作答的提问必然是最后一条非工具消息,
 * 从尾部跳过工具组看第一行即可,不必全量扫。
 */
export function hasActiveQuestion(rows: readonly MirrorRow[]): boolean {
  for (let i = rows.length - 1; i >= 0; i--) {
    const r = rows[i];
    if (r.kind === 'tools') continue;
    return r.kind === 'question' && r.active;
  }
  return false;
}

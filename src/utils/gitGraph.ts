import type { GitCommitInfo } from '../types';

/**
 * 提交历史拓扑图布局。
 *
 * 输入是按拓扑序（父提交永远在子提交之后）排好的 commit 列表，
 * 输出每一行要画的节点位置与连线段，供 SVG 逐行绘制。
 *
 * 算法：自上而下扫描，维护一组「lane」，每个 lane 记录它当前正在等待的
 * commit hash。扫到某个 commit 时，所有等待它的 lane 汇聚到节点上，
 * 再把它的父提交派发回 lane（第 0 个父继承节点所在 lane，其余父另开 lane）。
 */

/** 单个 lane 的水平间距（px） */
export const GRAPH_LANE_WIDTH = 14;
/** 每个 commit 行的固定高度（px）——连线要跨行接续，行高必须固定 */
export const GRAPH_ROW_HEIGHT = 48;
/** 最多渲染的 lane 数，超出的一律画在最后一列，避免图形区把文字挤没 */
export const GRAPH_MAX_LANES = 8;

const PALETTE = [
  '#58a6ff',
  '#3fb950',
  '#d29922',
  '#bc8cff',
  '#f78166',
  '#39c5cf',
  '#db61a2',
  '#a5d6ff',
];

export interface GraphSegment {
  /** 线段在本行顶边所处的 lane；-1 表示它从本行节点出发（只画下半程） */
  from: number;
  /** 线段在本行底边所处的 lane；-1 表示它终止于本行节点（只画上半程） */
  to: number;
  color: string;
}

export interface GraphRow {
  /** 节点所在 lane */
  lane: number;
  color: string;
  /** 合并提交（父数 ≥ 2）画空心圆以示区分 */
  isMerge: boolean;
  segments: GraphSegment[];
}

export interface GraphLayout {
  /** 与传入 commits 一一对应 */
  rows: GraphRow[];
  /** 图形区宽度（px） */
  width: number;
}

export function computeGitGraph(commits: GitCommitInfo[]): GraphLayout {
  type Lane = { hash: string; color: string } | null;

  const lanes: Lane[] = [];
  const rows: GraphRow[] = [];
  let colorSeq = 0;
  let maxLane = 0;

  const nextColor = () => PALETTE[colorSeq++ % PALETTE.length];

  const allocLane = (): number => {
    const idx = lanes.indexOf(null);
    if (idx >= 0) return idx;
    lanes.push(null);
    return lanes.length - 1;
  };

  for (const commit of commits) {
    const segments: GraphSegment[] = [];

    // 1. 找出所有正等待本 commit 的 lane
    const incoming: number[] = [];
    for (let i = 0; i < lanes.length; i++) {
      if (lanes[i]?.hash === commit.hash) incoming.push(i);
    }

    // 2. 节点落在最左侧的那条 incoming lane；没有则新开一条（分支尖端）
    let lane: number;
    let color: string;
    if (incoming.length > 0) {
      lane = incoming[0];
      color = lanes[lane]!.color;
    } else {
      lane = allocLane();
      color = nextColor();
      lanes[lane] = { hash: commit.hash, color };
    }

    // 3. 与本 commit 无关的 lane 直穿本行
    for (let i = 0; i < lanes.length; i++) {
      if (lanes[i] && i !== lane && !incoming.includes(i)) {
        segments.push({ from: i, to: i, color: lanes[i]!.color });
      }
    }

    // 4. 上半程：incoming 的每条线汇入节点；除节点所在 lane 外全部释放
    for (const i of incoming) {
      segments.push({ from: i, to: -1, color: lanes[i]!.color });
      if (i !== lane) lanes[i] = null;
    }

    // 5. 下半程：把父提交派发回 lane。先释放自己这条 lane，等第 0 个父认领。
    lanes[lane] = null;
    const parents = commit.parentHashes ?? [];
    for (let pi = 0; pi < parents.length; pi++) {
      const parent = parents[pi];
      // 该父提交已经有线在等它 → 本行直接汇过去，不另开 lane
      const existing = lanes.findIndex((l) => l?.hash === parent);
      if (existing >= 0) {
        segments.push({ from: -1, to: existing, color: lanes[existing]!.color });
        continue;
      }
      const target = pi === 0 ? lane : allocLane();
      const c = pi === 0 ? color : nextColor();
      lanes[target] = { hash: parent, color: c };
      segments.push({ from: -1, to: target, color: c });
    }
    // parents 为空（根提交）时 lanes[lane] 保持 null，线到此为止

    let rowMax = lane;
    for (const s of segments) rowMax = Math.max(rowMax, s.from, s.to);
    maxLane = Math.max(maxLane, rowMax);

    rows.push({ lane, color, isMerge: parents.length >= 2, segments });
  }

  const laneCount = Math.min(maxLane + 1, GRAPH_MAX_LANES);
  return { rows, width: laneCount * GRAPH_LANE_WIDTH + 4 };
}

/** lane 索引 → SVG 内 x 坐标（lane 中心） */
export function laneX(lane: number): number {
  const clamped = Math.min(lane, GRAPH_MAX_LANES - 1);
  return clamped * GRAPH_LANE_WIDTH + GRAPH_LANE_WIDTH / 2;
}

/** 拐角圆角半径 */
const R = 5;

/** 把一条线段编译成 SVG path */
export function segmentPath(seg: GraphSegment, nodeLane: number): string {
  const h = GRAPH_ROW_HEIGHT;
  const mid = h / 2;

  // 直穿整行
  if (seg.from >= 0 && seg.to >= 0) {
    const xf = laneX(seg.from);
    const xt = laneX(seg.to);
    if (xf === xt) return `M ${xf} 0 V ${h}`;
    return `M ${xf} 0 C ${xf} ${mid} ${xt} ${mid} ${xt} ${h}`;
  }

  const xn = laneX(nodeLane);

  // 上半程：从顶边的某条 lane 汇入节点
  if (seg.from >= 0) {
    const xf = laneX(seg.from);
    if (xf === xn) return `M ${xf} 0 V ${mid}`;
    const s = xn > xf ? 1 : -1;
    return `M ${xf} 0 V ${mid - R} Q ${xf} ${mid} ${xf + s * R} ${mid} H ${xn}`;
  }

  // 下半程：从节点分出到底边的某条 lane
  if (seg.to >= 0) {
    const xt = laneX(seg.to);
    if (xt === xn) return `M ${xn} ${mid} V ${h}`;
    const s = xt > xn ? 1 : -1;
    return `M ${xn} ${mid} H ${xt - s * R} Q ${xt} ${mid} ${xt} ${mid + R} V ${h}`;
  }

  return '';
}

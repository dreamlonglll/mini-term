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
  /**
   * 线段末端要融入的颜色。分支线并入主线（或汇进异色节点）时，
   * 两种颜色硬碰会很突兀，设了这个就沿路径渐变过去。与 color 相同时不渐变。
   */
  endColor?: string;
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
      segments.push({ from: i, to: -1, color: lanes[i]!.color, endColor: color });
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
        // 用本节点的颜色而非目标 lane 的颜色——线的颜色跟着分支走，
        // 一条分支线从诞生到汇入主线全程保持自己的颜色，只在根部渐变融入主线。
        segments.push({ from: -1, to: existing, color, endColor: lanes[existing]!.color });
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

/**
 * 贝塞尔控制点到端点的垂直距离。
 * 控制点摆在端点正上/正下方，两端切线就都是垂直的：
 * 线从节点垂直出发、垂直并入相邻行的 lane，行与行之间接得上，
 * 中段自然地斜向目标 lane（弧朝上凹）。
 */
const CURVE = GRAPH_ROW_HEIGHT / 4;

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
    return `M ${xf} 0 C ${xf} ${CURVE} ${xn} ${CURVE} ${xn} ${mid}`;
  }

  // 下半程：从节点分出到底边的某条 lane
  if (seg.to >= 0) {
    const xt = laneX(seg.to);
    if (xt === xn) return `M ${xn} ${mid} V ${h}`;
    return `M ${xn} ${mid} C ${xn} ${mid + CURVE} ${xt} ${mid + CURVE} ${xt} ${h}`;
  }

  return '';
}

/** 线段是否需要渐变（两端异色才需要） */
export function needsGradient(seg: GraphSegment): boolean {
  return !!seg.endColor && seg.endColor !== seg.color;
}

/** 渐变的起止坐标，与 segmentPath 的两个端点对齐 */
export function segmentGradient(
  seg: GraphSegment,
  nodeLane: number,
): { x1: number; y1: number; x2: number; y2: number } {
  const h = GRAPH_ROW_HEIGHT;
  const mid = h / 2;
  const xn = laneX(nodeLane);

  // 上半程：顶边 → 节点
  if (seg.from >= 0 && seg.to < 0) {
    return { x1: laneX(seg.from), y1: 0, x2: xn, y2: mid };
  }
  // 下半程：节点 → 底边
  if (seg.from < 0 && seg.to >= 0) {
    return { x1: xn, y1: mid, x2: laneX(seg.to), y2: h };
  }
  return { x1: laneX(seg.from), y1: 0, x2: laneX(seg.to), y2: h };
}

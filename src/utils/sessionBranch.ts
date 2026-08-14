import type { AiSession, LineageEdge } from '../types';

/**
 * 会话分支的纯逻辑层（node --test 直测，不 import UI/tauri）。
 * 设计: docs/plans/2026-08-14-session-branch-tree-design.md
 *
 * 两块内容：
 * 1. AGENT_BRANCH_CAPS —— 各 agent 的分支能力位（fork / resume 命令模板）。
 *    UI 按位显隐；新接一家 AI 只需在此声明。命令模板内置 sessionId 白名单
 *    校验，识别不了的一律不产出命令（与 aiResume 的「宁可不续也不敲错」同则）。
 * 2. buildSessionTree —— 会话平铺列表 + 分支边 → 森林。悬空父（父会话不在
 *    列表里，可能被清理或超出扫描窗口）落为根；环防御（磁盘数据不可信）。
 */

/** 与 aiResume 同一口径的会话 id 白名单（uuid / codex 线程 id 均落在其中） */
const SESSION_ID_RE = /^[A-Za-z0-9_-]+$/;

export interface AgentBranchCaps {
  /** 在新 PTY 中把 sessionId fork 成新会话的命令 */
  forkCommand: (sessionId: string) => string | null;
  /** 在新 PTY 中恢复(接管)sessionId 的命令 */
  resumeCommand: (sessionId: string) => string | null;
}

function guarded(template: (id: string) => string): (id: string) => string | null {
  return (id) => (SESSION_ID_RE.test(id) ? template(id) : null);
}

/**
 * 分支能力表。grok 刻意缺席：无 CLI 级 fork，`grok --resume` 是接管原会话
 * 而非复制，两处同写有风险；opencode/pi 无会话记录，同样缺席。
 */
export const AGENT_BRANCH_CAPS: Record<string, AgentBranchCaps> = {
  claude: {
    forkCommand: guarded((id) => `claude --resume ${id} --fork-session`),
    resumeCommand: guarded((id) => `claude --resume ${id}`),
  },
  codex: {
    forkCommand: guarded((id) => `codex fork ${id}`),
    resumeCommand: guarded((id) => `codex resume ${id}`),
  },
};

/**
 * agent 标识 → 能力表，与 buildResumeCommand 同一归一化口径：codex/grok 显式
 * 分流，**其余一律按 Claude**（hook 上报的标识是 `claude-code`，不是 `claude`；
 * AiSessionRef 的约定即「agent 缺省按 Claude 处理」）。grok 无 CLI 级 fork、
 * opencode/pi 无会话记录，显式排除。
 */
export function branchCapsForAgent(agent: string | undefined): AgentBranchCaps | null {
  const a = (agent ?? 'claude').toLowerCase();
  if (a === 'codex') return AGENT_BRANCH_CAPS.codex;
  if (a === 'grok' || a === 'opencode' || a === 'pi') return null;
  return AGENT_BRANCH_CAPS.claude;
}

export interface SessionTreeNode {
  session: AiSession;
  /** 到父会话的边（undefined = 根） */
  edge?: LineageEdge;
  children: SessionTreeNode[];
  /** 树深（根 = 0），渲染缩进用 */
  depth: number;
}

/**
 * 合并磁盘扫描边与自记账边：按 child id 去重，磁盘（disk）优先 ——
 * 磁盘指针是 CLI 亲写的权威，自记账只兜文件未落盘的窗口期。
 */
export function mergeLineageEdges(disk: LineageEdge[], bookkept: LineageEdge[]): LineageEdge[] {
  const byChild = new Map<string, LineageEdge>();
  for (const e of bookkept) byChild.set(e.sessionId, e);
  for (const e of disk) byChild.set(e.sessionId, e);
  return [...byChild.values()];
}

/**
 * 平铺会话 + 边 → 森林。保持 sessions 的输入顺序作为根序（调用方已按时间
 * 排好）；子节点按 timestamp 升序（先岔的在上）。
 *
 * 悬空父：边指向的父不在 sessions 里 → 子按根处理（列表有扫描窗口上限，
 * 父被清理或挤出窗口不该让子消失）。环：沿父链回到自身 → 该边按悬空处理。
 */
export function buildSessionTree(sessions: AiSession[], edges: LineageEdge[]): SessionTreeNode[] {
  const parentOf = new Map<string, LineageEdge>();
  // 自指边在建图时即丢弃：留着会让后代的父链游走误判成环
  for (const e of mergeLineageEdges(edges, [])) {
    if (e.parentSessionId !== e.sessionId) parentOf.set(e.sessionId, e);
  }

  const byId = new Map(sessions.map((s) => [s.id, s]));

  /** 判定有效父：在列表内、非自指、且沿父链不回到 id（环防御） */
  const effectiveParent = (id: string): string | null => {
    const seen = new Set<string>([id]);
    let cur = parentOf.get(id);
    if (!cur || !byId.has(cur.parentSessionId) || cur.parentSessionId === id) return null;
    // 沿链走到根，途中重逢即成环
    let hop = cur.parentSessionId;
    while (true) {
      if (seen.has(hop)) return null;
      seen.add(hop);
      const next = parentOf.get(hop);
      if (!next || !byId.has(next.parentSessionId)) break;
      hop = next.parentSessionId;
    }
    return cur.parentSessionId;
  };

  const nodes = new Map<string, SessionTreeNode>();
  for (const s of sessions) {
    nodes.set(s.id, { session: s, children: [], depth: 0 });
  }

  const roots: SessionTreeNode[] = [];
  for (const s of sessions) {
    const node = nodes.get(s.id)!;
    const parentId = effectiveParent(s.id);
    if (parentId) {
      node.edge = parentOf.get(s.id);
      nodes.get(parentId)!.children.push(node);
    } else {
      roots.push(node);
    }
  }

  // 子按时间升序（先岔的在上）；根保持输入顺序
  const sortChildren = (node: SessionTreeNode, depth: number) => {
    node.depth = depth;
    node.children.sort((a, b) => a.session.timestamp.localeCompare(b.session.timestamp));
    for (const c of node.children) sortChildren(c, depth + 1);
  };
  for (const r of roots) sortChildren(r, 0);
  return roots;
}

/** 取 sessionId 所在家族的根（pane 浮层画整个家族用）。找不到返回 null。 */
export function findFamilyRoot(roots: SessionTreeNode[], sessionId: string): SessionTreeNode | null {
  const contains = (node: SessionTreeNode): boolean =>
    node.session.id === sessionId || node.children.some(contains);
  return roots.find(contains) ?? null;
}

export interface FlatSessionRow {
  node: SessionTreeNode;
  /** 渲染在行首的连线前缀（制表符：│ ├ └），根为空串。等宽字体下对齐。 */
  prefix: string;
}

/** 森林 → 带连线前缀的平铺行（先根深度优先，与视觉树一致）。 */
export function flattenSessionTree(roots: SessionTreeNode[]): FlatSessionRow[] {
  const out: FlatSessionRow[] = [];
  // ancestorsLast[i] = 第 i 层祖先是否是其父的最后一个孩子（决定画 │ 还是留白）
  const walk = (node: SessionTreeNode, ancestorsLast: boolean[]) => {
    const depth = ancestorsLast.length;
    let prefix = '';
    if (depth > 0) {
      for (let i = 0; i < depth - 1; i += 1) prefix += ancestorsLast[i] ? '   ' : '│  ';
      prefix += ancestorsLast[depth - 1] ? '└─ ' : '├─ ';
    }
    out.push({ node, prefix });
    node.children.forEach((c, i) => walk(c, [...ancestorsLast, i === node.children.length - 1]));
  };
  for (const r of roots) walk(r, []);
  return out;
}

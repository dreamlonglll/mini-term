/**
 * 浏览器历史栈与应用视图层级的同步规划(纯函数,零运行时依赖,
 * `tests/mobilePureLogic.test.cjs` 直接引用;真正调 History API 的是 `navigation.ts`)。
 *
 * 模型:应用的「逻辑深度」= 当前叠了几层可返回的视图(历史列表 / 历史预览 / 镜像 /
 * 终端视图 / 弹层,见 navigation.ts)。浏览器里每层对应一条我们 pushState 的历史记录,
 * 记录的 state 形如 `{ __mtNav: 1, depth: n }`(n = 该记录对应的逻辑深度,根 = 0)。
 * 只同步「深度」:返回永远是关掉最上面一层,所以不必记每条记录对应哪个视图。
 *
 * - 逻辑深度变大(用户点开了下一级)→ 补 pushState 到同样深度;
 * - 逻辑深度变小(点了页内的「‹ 返回」、切桌面端清空视图等)→ history.go(-差值),
 *   这次 go 触发的 popstate 是自己的,不再关视图;
 * - 用户按返回(popstate,不是自己的)→ 落到深度 d 的记录 → 关掉 (逻辑深度 - d) 层;
 * - state 不是我们的(脚注锚点 `#user-content-fn-1` 之类的片段跳转会新建一条 state=null 的
 *   记录)→ 不当导航,把这条记录标成当前逻辑深度,之后从它返回、往它返回都按普通记录处理;
 * - 落到比逻辑深度还深的记录(浏览器「前进」到已关掉的视图)→ 回退到逻辑深度,不复活视图。
 */

export const NAV_STATE_KEY = '__mtNav';

export interface NavEntryState {
  [NAV_STATE_KEY]: 1;
  depth: number;
}

export function makeNavState(depth: number): NavEntryState {
  return { [NAV_STATE_KEY]: 1, depth };
}

/** 读出历史记录 state 里的深度;不是本应用打的标记返回 null。 */
export function readNavDepth(state: unknown): number | null {
  if (!state || typeof state !== 'object') return null;
  const rec = state as Record<string, unknown>;
  if (rec[NAV_STATE_KEY] !== 1) return null;
  const depth = rec.depth;
  return typeof depth === 'number' && Number.isInteger(depth) && depth >= 0 ? depth : null;
}

export type SyncPlan =
  | { kind: 'none' }
  /** markCurrent 非 null:先把当前记录(未打标记的)标成该深度,再依次 push `depths` */
  | { kind: 'push'; markCurrent: number | null; depths: number[] }
  | { kind: 'go'; delta: number };

/**
 * 视图层级变了之后要对浏览器历史做什么。
 * `pendingGo > 0`:还有自己发起的 history.go 没落地,先等它的 popstate 再说。
 */
export function planSync(input: {
  logicalDepth: number;
  browserDepth: number;
  pendingGo: number;
  currentEntryDepth: number | null;
}): SyncPlan {
  const { logicalDepth, browserDepth, pendingGo, currentEntryDepth } = input;
  if (pendingGo > 0) return { kind: 'none' };
  if (logicalDepth > browserDepth) {
    const depths: number[] = [];
    for (let d = browserDepth + 1; d <= logicalDepth; d += 1) depths.push(d);
    return {
      kind: 'push',
      markCurrent: currentEntryDepth === null ? browserDepth : null,
      depths,
    };
  }
  if (logicalDepth < browserDepth) return { kind: 'go', delta: logicalDepth - browserDepth };
  return { kind: 'none' };
}

export interface PopPlan {
  /** 落地后浏览器所在记录的深度 */
  browserDepth: number;
  /** 非 null:把落地的记录(未打标记)标成这个深度 */
  mark: number | null;
  /** 要从上往下关掉几层视图 */
  close: number;
  /** 非 0:再 history.go(这么多) 纠正「前进」到的过深记录 */
  go: number;
  /** 这次 popstate 是不是自己 history.go 的落地(调用方据此把 pendingGo 减一) */
  ours: boolean;
}

/**
 * 收到 popstate 时的处理。
 * `goTarget`:自己那次 history.go 想落到的深度(落到未打标记的记录时按它标记)。
 */
export function planPop(input: {
  stateDepth: number | null;
  logicalDepth: number;
  pendingGo: number;
  goTarget: number;
}): PopPlan {
  const { stateDepth, logicalDepth, pendingGo, goTarget } = input;
  const ours = pendingGo > 0;
  if (ours) {
    // 自己的 go 落地:不关视图(视图早就关了),落地深度未知就按目标深度认
    const depth = stateDepth ?? goTarget;
    return { browserDepth: depth, mark: stateDepth === null ? depth : null, close: 0, go: 0, ours };
  }
  if (stateDepth === null) {
    // 片段跳转(脚注锚点)等非本应用的记录:不是导航,就地认作当前深度
    return { browserDepth: logicalDepth, mark: logicalDepth, close: 0, go: 0, ours };
  }
  if (stateDepth < logicalDepth) {
    return { browserDepth: stateDepth, mark: null, close: logicalDepth - stateDepth, go: 0, ours };
  }
  if (stateDepth > logicalDepth) {
    return { browserDepth: stateDepth, mark: null, close: 0, go: logicalDepth - stateDepth, ours };
  }
  return { browserDepth: stateDepth, mark: null, close: 0, go: 0, ours };
}

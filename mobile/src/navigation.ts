/**
 * 接入浏览器 History API:进入下一级视图时 pushState,Android 返回手势 / 返回键(popstate)
 * 关掉最上面一层,回到根视图(会话列表首页 / 全屏状态页)后再按返回才真正离开 PWA。
 * 同步规划是纯逻辑,见 `navStack.ts`;这里只负责读 store、调 History API。
 *
 * 可返回的层级,自下而上(与 App 的视图优先级一致,返回总是关最上面那层):
 *   1. 项目历史会话列表   historyList
 *   2. 历史会话只读预览   historyPreview
 *   3. 对话镜像           mirror
 *   4. 镜像里的终端视图   mirror.viewMode === 'terminal'(从对话切过去算进一级,切回对话即退一级)
 *   5. 弹层(切换桌面端 / 新建会话及其选启动器一步 / 重命名),由组件经 `useBackLayer` 登记
 * 首页的「活跃 / 历史」分段标签是同级切换,不进历史栈。
 *
 * 历史记录的 state 结构:`{ __mtNav: 1, depth: n }`(n = 这条记录对应的层数,根 = 0),
 * URL 一律是当前的 `pathname + search`(带 `?desktop=`,不带 hash)。
 *
 * 与 hash 共存:
 * - 配对 / 通知深链(`#pair=` / `#pane=`)由 connection.ts 的 hashchange 处理并当场清掉;
 *   pushState / replaceState 不触发 hashchange,本模块也从不往 URL 里写 hash,所以不会
 *   因为导航重放配对逻辑。auth.ts 清 hash / 改 `?desktop=` 时保留 history.state,标记不丢。
 * - Markdown 脚注锚点(`#user-content-fn-1`)是浏览器自己的片段跳转,会新建一条 state=null 的
 *   记录:popstate / hashchange 里把它标成当前层数,不关任何视图;从脚注处按返回回到原位置,
 *   仍是同一层,也不关视图。
 */
import { useEffect, useRef } from 'react';
import { makeNavState, planPop, planSync, readNavDepth } from './navStack';
import {
  closeHistoryPreview,
  closeMirror,
  closeProjectHistory,
  setMirrorViewMode,
  useRelayStore,
} from './relay';
import { readDesktopQuery, syncDesktopUrl } from './relay/auth';

type StoreState = ReturnType<typeof useRelayStore.getState>;

interface Layer {
  id: number;
  close: () => void;
}

/** 组件登记的弹层,按打开顺序;永远叠在 store 层级之上(都是全屏遮罩的模态弹层) */
const layers: Layer[] = [];
let nextLayerId = 1;

/** 浏览器当前所在记录的层数(本模块记账) */
let browserDepth = 0;
/** 自己发起、还没等到 popstate 的 history.go 次数 */
let pendingGo = 0;
/** 自己那次 history.go 想落到的层数 */
let goTarget = 0;
let goWatchdog: ReturnType<typeof setTimeout> | null = null;
let syncScheduled = false;
let initialized = false;

/** 等自己的 history.go 落地的上限:记录不存在 / 浏览器吞了这次 go 时据此解锁,不让同步永远卡住 */
const GO_WATCHDOG_MS = 1500;

function storeDepth(s: StoreState): number {
  let depth = 0;
  if (s.historyList) depth += 1;
  if (s.historyPreview) depth += 1;
  if (s.mirror) {
    depth += 1;
    if (s.mirror.viewMode === 'terminal') depth += 1;
  }
  return depth;
}

function logicalDepth(): number {
  return storeDepth(useRelayStore.getState()) + layers.length;
}

/** 关掉最上面一层。返回 false = 已在根视图,没有可关的。 */
function closeTopLevel(): boolean {
  const layer = layers.pop();
  if (layer) {
    layer.close();
    return true;
  }
  const s = useRelayStore.getState();
  if (s.mirror) {
    if (s.mirror.viewMode === 'terminal') setMirrorViewMode('chat');
    else closeMirror();
    return true;
  }
  if (s.historyPreview) {
    closeHistoryPreview();
    return true;
  }
  if (s.historyList) {
    closeProjectHistory();
    return true;
  }
  return false;
}

function replaceCurrentState(depth: number) {
  try {
    history.replaceState(makeNavState(depth), '');
  } catch {
    /* 极少数受限环境不给改:只影响返回键的层级感知,界面照常 */
  }
}

function clearGoWatchdog() {
  if (goWatchdog) {
    clearTimeout(goWatchdog);
    goWatchdog = null;
  }
}

function goBy(delta: number) {
  pendingGo += 1;
  goTarget = browserDepth + delta;
  clearGoWatchdog();
  goWatchdog = setTimeout(() => {
    goWatchdog = null;
    if (pendingGo === 0) return;
    pendingGo = 0;
    browserDepth = readNavDepth(history.state) ?? goTarget;
    scheduleSync();
  }, GO_WATCHDOG_MS);
  try {
    history.go(delta);
  } catch {
    pendingGo = Math.max(0, pendingGo - 1);
  }
}

function sync() {
  syncScheduled = false;
  const plan = planSync({
    logicalDepth: logicalDepth(),
    browserDepth,
    pendingGo,
    currentEntryDepth: readNavDepth(history.state),
  });
  if (plan.kind === 'push') {
    try {
      if (plan.markCurrent !== null) history.replaceState(makeNavState(plan.markCurrent), '');
      const url = window.location.pathname + window.location.search;
      for (const depth of plan.depths) {
        history.pushState(makeNavState(depth), '', url);
        browserDepth = depth;
      }
    } catch {
      /* pushState 被限流等:放弃这次同步,返回键退化为直接离开 */
    }
  } else if (plan.kind === 'go') {
    goBy(plan.delta);
  }
}

/**
 * 视图层级可能变了:放到微任务里统一同步。同一轮里「关一个再开一个」(深链换会话、
 * 预览页继续对话进镜像)层数不变,合并后就不会多出一次后退再前进。
 */
function scheduleSync() {
  if (syncScheduled) return;
  syncScheduled = true;
  queueMicrotask(sync);
}

/**
 * 返回落到早先的记录上,它的 `?desktop=` 可能是切换桌面端之前的;以当前桌面端为准改回来,
 * 免得刷新 / 收藏书签时连到另一台电脑。
 */
function canonicalizeDesktopQuery() {
  const id = useRelayStore.getState().currentDesktopId;
  if (readDesktopQuery() !== id) syncDesktopUrl(id);
}

function onPopState(ev: PopStateEvent) {
  const plan = planPop({
    stateDepth: readNavDepth(ev.state),
    logicalDepth: logicalDepth(),
    pendingGo,
    goTarget,
  });
  if (plan.ours) {
    pendingGo = Math.max(0, pendingGo - 1);
    if (pendingGo === 0) clearGoWatchdog();
  }
  browserDepth = plan.browserDepth;
  if (plan.mark !== null) replaceCurrentState(plan.mark);
  for (let i = 0; i < plan.close; i += 1) {
    if (!closeTopLevel()) break;
  }
  if (plan.go !== 0) goBy(plan.go);
  canonicalizeDesktopQuery();
  scheduleSync();
}

/** 片段跳转(脚注锚点、页面已开着时再点的深链)新建的记录没有标记:就地标成当前层数。 */
function onHashChange() {
  if (readNavDepth(history.state) !== null) return;
  const depth = logicalDepth();
  replaceCurrentState(depth);
  browserDepth = depth;
  scheduleSync();
}

/** 应用启动时调一次(早于 startRelay):给首条记录打上根标记并开始监听。 */
export function initNavigation() {
  if (initialized) return;
  initialized = true;
  replaceCurrentState(0);
  browserDepth = 0;
  useRelayStore.subscribe(scheduleSync);
  window.addEventListener('popstate', onPopState);
  window.addEventListener('hashchange', onHashChange);
}

function registerLayer(close: () => void): number {
  const id = nextLayerId;
  nextLayerId += 1;
  layers.push({ id, close });
  scheduleSync();
  return id;
}

function unregisterLayer(id: number) {
  const idx = layers.findIndex((l) => l.id === id);
  if (idx < 0) return; // 返回键关的:closeTopLevel 已先摘掉
  layers.splice(idx, 1);
  scheduleSync();
}

/**
 * 让一个弹层(或弹层里的一步)参与返回手势:`open` 为 true 期间占一级历史记录,
 * 按返回调 `onClose`;从界面上关掉(点取消 / 遮罩 / 选完)时自动退掉这一级记录。
 */
export function useBackLayer(open: boolean, onClose: () => void) {
  const closeRef = useRef(onClose);
  useEffect(() => {
    closeRef.current = onClose;
  });
  useEffect(() => {
    if (!open) return;
    const id = registerLayer(() => closeRef.current());
    return () => unregisterLayer(id);
  }, [open]);
}

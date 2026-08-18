import { useState, useRef, useEffect, useCallback } from 'react';
import { createPortal } from 'react-dom';
import { createRoot } from 'react-dom/client';
import { flushSync } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, clearPaneResumePendingByPty, setPaneAiSessionByPty, saveLayoutToConfig } from '../store';
import { TerminalInstance } from './TerminalInstance';
import { StatusDot } from './StatusDot';
import { BrandIcon } from './BrandIcon';
import { MarkerList } from './MarkerList';
import { PaneTabPreview } from './PaneTabPreview';
import { showContextMenu } from '../utils/contextMenu';
import { inferVendor } from '../utils/inferVendor';
import { paneShowsAiSession, resolveAutoResumeCommand } from '../utils/aiResume';
import { disposeTerminal, writePtyInput } from '../utils/terminalCache';
import { createProjectPty, isRemoteProject, paneDisplayLabel } from '../utils/remoteProject';
import { findPaneById } from '../utils/layoutOps';
import {
  activatePane,
  closeLeaf,
  closePane,
  forkPaneSession,
  movePane,
  movePaneToTab,
  newTerminal,
  renamePane,
  splitPane,
} from '../utils/paneActions';
import { getPaneDragPayload, initPaneDrag, PANE_DRAG_CANCEL_EVENT } from '../utils/paneDragState';
import type { DropZone } from '../utils/layoutOps';
import { branchCapsForAgent } from '../utils/sessionBranch';
import { BranchFamilyPanel } from './BranchFamilyPanel';
import { hotkeyLabel } from '../utils/hotkeys';
import { openTerminalSearch } from '../utils/terminalSearch';
import { MOD_LABEL } from '../utils/platform';
import { useT } from '../i18n';
import type { SplitNode, PaneState, ShellConfig, AiMarker } from '../types';

const EMPTY_MARKERS: AiMarker[] = [];
const hydratingPaneIds = new Set<string>();

// 分屏 / 关闭控件的图标：与 ActivityBar 同一套描边语言（16 viewBox、stroke currentColor），
// 取代原先的 ┃ ━ ✕ 字符 —— 字符既没有图形语义，字体不同还会跳。
const ICON_SPLIT_RIGHT = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round">
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M8 3v10" />
  </svg>
);
const ICON_SPLIT_DOWN = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round">
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M2 8h12" />
  </svg>
);
const ICON_CLOSE = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round">
    <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
  </svg>
);
const ICON_SEARCH = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round">
    <circle cx="7" cy="7" r="4.2" />
    <path d="M10.2 10.2L14 14" />
  </svg>
);
const ICON_MAXIMIZE = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
    <path d="M9.5 2.5h4v4M13.5 2.5L9 7M6.5 13.5h-4v-4M2.5 13.5L7 9" />
  </svg>
);
const ICON_RESTORE = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
    <path d="M13.5 6.5h-4v-4M9.5 6.5L14 2M2.5 9.5h4v4M6.5 9.5L2 14" />
  </svg>
);

interface Props {
  projectId: string;
  node: SplitNode & { type: 'leaf' };
  projectPath: string;
}

export function PaneGroup({ projectId, node, projectPath }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setPanePty = useAppStore((s) => s.setPanePty);
  const updatePaneStatusByPaneId = useAppStore((s) => s.updatePaneStatusByPaneId);
  // create_pty 失败时的错误详情(按 paneId 记录),远程断链 / 缺 ssh 客户端时展示明确原因
  const [spawnErrors, setSpawnErrors] = useState<Record<string, string>>({});

  const activePane = node.panes.find((p) => p.id === node.activePaneId) ?? node.panes[0];

  // SSH 远程项目:所有 pane 统一按远程方式启动(布局恢复亦然);
  // pane 显示名走 paneDisplayLabel 统一口径(恢复布局时 shellName 会被映射为本地 shell 名,不可信)。
  const project = config.projects.find((p) => p.id === projectId);
  const remote = isRemoteProject(project);
  const paneLabel = (pane: PaneState) => paneDisplayLabel(pane, project);
  // 缺省开启;既决定起 PTY 时写不写 resume,也决定待续接的 pane 亮不亮 AI 图标
  const autoResumeEnabled = config.aiAutoResume ?? true;

  useEffect(() => {
    if (!activePane || activePane.ptyId !== undefined || activePane.status === 'error') return;
    if (hydratingPaneIds.has(activePane.id)) return;
    if (!project) return;

    let shell: ShellConfig | undefined;
    if (!remote) {
      shell = config.availableShells.find((s) => s.name === activePane.shellName)
        ?? config.availableShells.find((s) => s.name === config.defaultShell)
        ?? config.availableShells[0];
      if (!shell) {
        updatePaneStatusByPaneId(projectId, activePane.id, 'error');
        return;
      }
    }

    hydratingPaneIds.add(activePane.id);
    // 续接会话时 PTY 直接以会话记录的 cwd 启动:claude --resume 只认「启动目录」
    // 对应的会话桶,起于子目录的会话在项目根恢复会报 No conversation found。
    // 存量记录无 cwd 时向后端反查 jsonl(lookup_ai_session_cwd),查到随身份写回
    // 持久化,下次重启免查;codex 会话不按目录分桶,无需反查。
    //
    // 与 resolveAutoResumeCommand 消费同一个 enabled:resumePending 在开关关闭时
    // 是**刻意保留**的(见 aiResume.ts),只看它会让关掉自动续接的用户拿到「不写
    // resume 命令、shell 却开在会话子目录里」的割裂结果。开关同样取现值而非
    // autoResumeEnabled 快照(config 不在 effect deps 里,理由见下方 resumeCmd 处)。
    const sessionRef =
      (useAppStore.getState().config.aiAutoResume ?? true) && activePane.resumePending
        ? activePane.aiSession
        : undefined;
    const resolveResumeCwd = async (): Promise<string | undefined> => {
      if (remote || !sessionRef) return undefined;
      if (sessionRef.cwd) return sessionRef.cwd;
      if (sessionRef.agent === 'codex') return undefined;
      if (!/^[A-Za-z0-9_-]+$/.test(sessionRef.sessionId)) return undefined;
      const found = await invoke<string | null>('lookup_ai_session_cwd', {
        sessionId: sessionRef.sessionId,
      }).catch(() => null);
      return found ?? undefined;
    };
    let resumeCwd: string | undefined;
    // 远程分支:create_pty 带 sshRemote,后端直接 spawn ssh 并预注册密码 autofill;
    // 本地分支:行为与既有链路一致(shell + cwd + envVars),pane 的 cwd 覆盖优先(worktree 终端)。
    resolveResumeCwd()
      .then((cwd) => {
        resumeCwd = cwd;
        // pane 自己的 cwd 优先:那是用户显式给这个 pane 定的目录(worktree 终端
        // 靠它),会话 cwd 只在 pane 没指定时兜底。反过来写会让续接把 worktree
        // 终端带偏到会话记录的目录去。
        const startCwd = activePane.cwd ?? cwd;
        if (startCwd === undefined || startCwd === activePane.cwd) {
          return createProjectPty(project, shell, startCwd);
        }
        // 会话 cwd 是持久化下来的旧值,目录可能在这期间没了(worktree 移除、
        // 项目搬家)。那本是「续接得更准」的优化,不该把 pane 拖成 error ——
        // 失败就退回项目默认目录重来一次,大不了 resume 找不到会话桶。
        return createProjectPty(project, shell, startCwd).catch((e) => {
          console.warn(`以会话目录 ${startCwd} 启动失败，回落项目默认目录:`, e);
          resumeCwd = undefined;
          return createProjectPty(project, shell, activePane.cwd);
        });
      })
      .then((ptyId) => {
        const layout = useAppStore.getState().projectStates.get(projectId)?.layout;
        const pane = layout ? findPaneById(layout, activePane.id) : null;
        if (pane && pane.ptyId === undefined) {
          setPanePty(projectId, activePane.id, ptyId);
          // 恢复的布局带待续接标记 → 写 resume 命令续接上次会话(否决条件见
          // resolveAutoResumeCommand:系统设置开关 / 待续接标记 / 远程 pane / id 白名单)。
          // PTY 内核缓冲 stdin,shell 就绪前写入不丢(与移动端发起会话同一时序)。
          // 只清 resumePending 标记、**保留 aiSession 身份**:codex resume 不会
          // 重新上报 SessionStart,若写完把身份也清掉,第二次重启就断代恢复不了;
          // claude 会上报新身份自然覆盖旧值。会话身份取 effect 闭包快照,
          // 规避 monitor 在 create 与 write 之间发 idle 把身份抹掉的竞态。
          const resumeCmd = resolveAutoResumeCommand({
            // 开关取现值而非 autoResumeEnabled:config 不在 effect deps 里,
            // 闭包快照可能已过期(用户在起 PTY 期间刚改了设置)
            enabled: useAppStore.getState().config.aiAutoResume ?? true,
            resumePending: pane.resumePending,
            session: activePane.aiSession,
            remote,
          });
          if (resumeCmd) {
            clearPaneResumePendingByPty(ptyId);
            void writePtyInput(ptyId, `${resumeCmd}\r`);
            // 反查所得的启动目录随身份写回并持久化,下次重启直达不再查
            const session = activePane.aiSession;
            if (resumeCwd && session && session.cwd !== resumeCwd) {
              setPaneAiSessionByPty(ptyId, { ...session, cwd: resumeCwd });
              saveLayoutToConfig(projectId);
            }
          }
          setSpawnErrors((prev) => {
            if (!(activePane.id in prev)) return prev;
            const next = { ...prev };
            delete next[activePane.id];
            return next;
          });
        } else {
          invoke('kill_pty', { ptyId }).catch(() => {});
        }
      })
      .catch((e) => {
        setSpawnErrors((prev) => ({
          ...prev,
          [activePane.id]: e instanceof Error ? e.message : String(e),
        }));
        updatePaneStatusByPaneId(projectId, activePane.id, 'error');
      })
      .finally(() => {
        hydratingPaneIds.delete(activePane.id);
      });
  }, [
    activePane?.id,
    activePane?.ptyId,
    activePane?.shellName,
    activePane?.status,
    activePane?.cwd,
    activePane?.aiSession,
    activePane?.resumePending,
    config.availableShells,
    config.defaultShell,
    project,
    remote,
    projectId,
    projectPath,
    setPanePty,
    updatePaneStatusByPaneId,
  ]);

  const handleNewTabClick = useCallback((e: React.MouseEvent) => {
    // 远程项目不弹 shell 菜单:pane 固定为 ssh 启动器
    if (remote || config.availableShells.length <= 1) {
      void newTerminal(projectId, undefined, { targetPaneId: activePane?.id });
      return;
    }
    showContextMenu(
      e.clientX,
      e.clientY,
      config.availableShells.map((shell) => ({
        label: shell.name,
        onClick: () => void newTerminal(projectId, shell, { targetPaneId: activePane?.id }),
      })),
    );
  }, [remote, config.availableShells, projectId, activePane?.id]);

  // === 非激活 tab 悬停缩略图:250ms 后弹出,移出/点击/滚动即关(时序同项目行预览) ===
  const [tabPreview, setTabPreview] = useState<{
    paneId: string;
    rect: { left: number; top: number; bottom: number };
  } | null>(null);
  const tabPreviewTimer = useRef<ReturnType<typeof setTimeout>>(undefined);

  const closeTabPreview = useCallback(() => {
    clearTimeout(tabPreviewTimer.current);
    setTabPreview(null);
  }, []);

  const handleTabPreviewEnter = useCallback((e: React.MouseEvent, paneId: string) => {
    clearTimeout(tabPreviewTimer.current);
    // currentTarget 只在事件分发期间有效,先留住 DOM 引用;rect 到点弹时再取
    const el = e.currentTarget as HTMLElement;
    tabPreviewTimer.current = setTimeout(() => {
      if (!el.isConnected) return;
      const r = el.getBoundingClientRect();
      setTabPreview({ paneId, rect: { left: r.left, top: r.top, bottom: r.bottom } });
    }, 250);
  }, []);

  useEffect(() => () => clearTimeout(tabPreviewTimer.current), []);

  // 渲染 gate 之外把 state 也收掉(与 ProjectList 同一双闸模式):用 X 关掉被
  // 悬停的 tab 时点击被 stopPropagation 拦下,closeTabPreview 不触发,旧锚点
  // 会一直残留到下次悬停
  useEffect(() => {
    if (!tabPreview) return;
    const pane = node.panes.find((p) => p.id === tabPreview.paneId);
    if (!pane || pane.id === activePane?.id) closeTabPreview();
  }, [tabPreview, node.panes, activePane?.id, closeTabPreview]);

  // tab 栏可横向滚动,布局变化时锚点失效,直接关闭
  useEffect(() => {
    if (!tabPreview) return;
    window.addEventListener('scroll', closeTabPreview, true);
    window.addEventListener('wheel', closeTabPreview, { passive: true });
    return () => {
      window.removeEventListener('scroll', closeTabPreview, true);
      window.removeEventListener('wheel', closeTabPreview);
    };
  }, [tabPreview, closeTabPreview]);

  const [markerOpen, setMarkerOpen] = useState(false);
  const [markerAnchor, setMarkerAnchor] = useState<{ top: number; right: number } | null>(null);
  const markers = useAppStore(
    (s) => (activePane?.ptyId !== undefined && s.markersByPty.get(activePane.ptyId)) || EMPTY_MARKERS,
  );
  const markerBtnRef = useRef<HTMLButtonElement>(null);
  const markerPopoverRef = useRef<HTMLDivElement>(null);

  const openMarkerPopover = useCallback(() => {
    const rect = markerBtnRef.current?.getBoundingClientRect();
    if (!rect) return;
    setMarkerAnchor({ top: rect.bottom + 4, right: window.innerWidth - rect.right });
    setMarkerOpen(true);
  }, []);

  useEffect(() => {
    if (!markerOpen) return;
    const onDoc = (e: MouseEvent) => {
      const target = e.target as Node;
      if (markerPopoverRef.current?.contains(target)) return;
      if (markerBtnRef.current?.contains(target)) return;
      setMarkerOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [markerOpen]);

  useEffect(() => {
    setMarkerOpen(false);
  }, [activePane?.ptyId]);

  const handleRetryCreatePty = useCallback(() => {
    if (!activePane) return;
    updatePaneStatusByPaneId(projectId, activePane.id, 'idle');
  }, [activePane, projectId, updatePaneStatusByPaneId]);

  // 远程 pane 断线检测:ssh 进程退出(pty-exit,不区分用户 exit 与异常断线)后
  // pane 不自动关闭,叠加「连接已断开,点击重连」覆盖层。
  const exitedPtyIds = useAppStore((s) => s.exitedPtyIds);
  const showReconnect =
    remote && activePane?.ptyId !== undefined && exitedPtyIds.has(activePane.ptyId);

  // 重连:同一 pane 重新 create_pty(清屏方案 —— 销毁旧 xterm 实例,复用懒创建
  // effect 重建全新终端)。选清屏而非保留历史:新 PTY 的输出从头开始,旧 buffer 的
  // 光标/滚动状态与新会话无法衔接,保留反而会出现«半屏旧内容 + 新登录横幅»的错位;
  // 且 dispose 一并回收 markers/WebGL 资源,链路与关 tab 完全一致,无新状态机。
  const handleReconnect = useCallback(() => {
    if (!activePane || activePane.ptyId === undefined) return;
    const oldPtyId = activePane.ptyId;
    invoke('kill_pty', { ptyId: oldPtyId }).catch(() => {});
    disposeTerminal(oldPtyId);
    useAppStore.getState().clearMarkersForPty(oldPtyId);
    useAppStore.getState().clearPtyExited(oldPtyId);
    // 置 ptyId=undefined + status=idle → 懒创建 effect 走远程分支重新 spawn ssh
    useAppStore.getState().resetPaneForReconnect(projectId, activePane.id);
  }, [activePane, projectId]);

  // === pane 拖拽移动/合并 + 双击最大化 ===
  // 拖拽走 paneDragState 的 mousedown/mousemove/mouseup 自绘链路（与项目行、
  // 文件树拖拽同一模式）——Tauri dragDropEnabled 拦截了 WebView 内部的
  // HTML5 DnD 事件，draggable/dragover/drop 在本应用里根本不触发。
  const [dropZone, setDropZone] = useState<DropZone | null>(null);
  // tab 栏拖拽悬停的插入位：index 用于落子，x 是指示线的容器内容坐标
  const [tabDrop, setTabDrop] = useState<{ index: number; x: number } | null>(null);
  const maximizedPaneId = useAppStore((s) => s.projectStates.get(projectId)?.maximizedPaneId);
  const layoutIsSplit = useAppStore(
    (s) => s.projectStates.get(projectId)?.layout?.type === 'split',
  );
  const isMaximized = maximizedPaneId !== undefined
    && node.panes.some((p) => p.id === maximizedPaneId);

  const toggleMaximize = useCallback(() => {
    const store = useAppStore.getState();
    if (node.panes.some((p) => p.id === store.projectStates.get(projectId)?.maximizedPaneId)) {
      store.toggleMaximizedPane(projectId, null);
    } else if (store.projectStates.get(projectId)?.layout?.type === 'split') {
      store.toggleMaximizedPane(projectId, activePane?.id ?? null);
    }
  }, [projectId, node.panes, activePane?.id]);

  /** 本组是否可作为当前拖拽 pane 的落点（同项目的 pane 拖拽才响应）。 */
  const acceptsPaneDrag = useCallback(() => {
    const p = getPaneDragPayload();
    return p && p.projectId === projectId ? p : null;
  }, [projectId]);

  // Esc 取消拖拽时鼠标不动、收不到 mouseleave，高亮靠取消事件撤下来
  useEffect(() => {
    const clear = () => {
      setDropZone(null);
      setTabDrop(null);
    };
    window.addEventListener(PANE_DRAG_CANCEL_EVENT, clear);
    return () => window.removeEventListener(PANE_DRAG_CANCEL_EVENT, clear);
  }, []);

  const zoneFromEvent = (e: React.MouseEvent): DropZone => {
    const r = e.currentTarget.getBoundingClientRect();
    const x = (e.clientX - r.left) / Math.max(r.width, 1);
    const y = (e.clientY - r.top) / Math.max(r.height, 1);
    // 四边各 1/4 进深算分屏落点，中间一块并入 tab 组
    if (x >= 0.25 && x <= 0.75 && y >= 0.25 && y <= 0.75) return 'center';
    const d = [
      { zone: 'left' as const, v: x },
      { zone: 'right' as const, v: 1 - x },
      { zone: 'top' as const, v: y },
      { zone: 'bottom' as const, v: 1 - y },
    ];
    d.sort((a, b) => a.v - b.v);
    return d[0].zone;
  };

  const handleBodyDragMove = (e: React.MouseEvent) => {
    if (!acceptsPaneDrag()) return;
    const zone = zoneFromEvent(e);
    setDropZone((prev) => (prev === zone ? prev : zone));
  };

  const handleBodyDrop = (e: React.MouseEvent) => {
    const payload = acceptsPaneDrag();
    setDropZone(null);
    if (!payload) return;
    movePane(projectId, payload.paneId, node.activePaneId, zoneFromEvent(e));
  };

  /** 由指针位置算 tab 栏插入位：落在某 tab 中线左侧即插到它前面，否则继续向右。 */
  const tabDropFromEvent = (e: React.MouseEvent): { index: number; x: number } => {
    const bar = e.currentTarget as HTMLElement;
    const barRect = bar.getBoundingClientRect();
    const tabs = Array.from(bar.querySelectorAll<HTMLElement>('[data-pane-tab]'));
    let index = tabs.length;
    let x = 0;
    for (let i = 0; i < tabs.length; i++) {
      const r = tabs[i].getBoundingClientRect();
      if (e.clientX < r.left + r.width / 2) {
        index = i;
        x = r.left - barRect.left + bar.scrollLeft;
        break;
      }
      x = r.right - barRect.left + bar.scrollLeft;
    }
    return { index, x };
  };

  const handleTabBarDragMove = (e: React.MouseEvent) => {
    const payload = acceptsPaneDrag();
    if (!payload) return;
    // 本组只有被拖的这一个 tab 时组内换位无意义，落下也不会有动作——
    // 不显示插入指示线，避免「指示了却静默无动作」的口径不一致
    if (!node.panes.some((p) => p.id !== payload.paneId)) return;
    const next = tabDropFromEvent(e);
    setTabDrop((prev) =>
      prev && prev.index === next.index && prev.x === next.x ? prev : next,
    );
  };

  // 不能 stopPropagation：paneDragState 的收尾（清拖拽态/卸监听/抑制 click）
  // 挂在 document 的 mouseup 上，截断传播会让拖拽态永久残留，
  // 表现为落子后高亮框跟着鼠标走、下一次点击又移动一遍
  const handleTabBarDrop = (e: React.MouseEvent) => {
    const payload = acceptsPaneDrag();
    setTabDrop(null);
    if (!payload) return;
    // anchor 只为定位本 leaf，不能是被拖的 pane 自己；找不到说明本组只有
    // 被拖的这一个 tab，组内换位无意义
    const anchor = node.panes.find((p) => p.id !== payload.paneId);
    if (!anchor) return;
    movePaneToTab(projectId, payload.paneId, anchor.id, tabDropFromEvent(e).index);
  };

  const paneContextMenu = useCallback((e: React.MouseEvent, paneId: string) => {
    e.preventDefault();
    e.stopPropagation();
    const { clientX, clientY } = e;
    // 会话分支入口:pane 有 AI 会话身份且该 agent 有 fork 能力位才出现
    // (claude/codex;grok 仅 resume 位无 fork,opencode/pi 无会话记录,均隐藏)
    const pane = node.panes.find((p) => p.id === paneId);
    const session = pane?.aiSession;
    const canFork = !!session && !!branchCapsForAgent(session.agent)?.forkCommand?.(session.sessionId);
    // 输入检测认出 AI 在跑但没有 hook 身份(hook 未注册/身份未达)——置灰提示
    // 原因,不再静默消失让人以为功能坏了;锚定 fork 位,仅 resume 的 grok 不提示
    const identityMissing = !session && !!pane?.detectedAgent
      && !!branchCapsForAgent(pane.detectedAgent)?.forkCommand;
    showContextMenu(clientX, clientY, [
      { label: t('paneGroup.rename'), shortcut: hotkeyLabel('renamePane'), onClick: () => void renamePane(projectId, paneId) },
      { separator: true },
      { label: t('paneGroup.splitRight'), shortcut: hotkeyLabel('splitRight'), onClick: () => void splitPane(projectId, 'horizontal', paneId) },
      { label: t('paneGroup.splitDown'), shortcut: hotkeyLabel('splitDown'), onClick: () => void splitPane(projectId, 'vertical', paneId) },
      ...(canFork && session ? [
        { separator: true as const },
        // 新 pane 是新进程,「本会话允许」的权限授权不迁移(官方行为)
        { label: t('paneGroup.forkSession'), onClick: () => void forkPaneSession(projectId, paneId) },
        {
          label: t('paneGroup.viewSessionBranches'),
          // 悬停展开家族树面板(连线/标题/厂商图标由 React 渲染)。
          // flushSync 同步出首帧,contextMenu 依赖真实尺寸定位
          submenuRender: (host: HTMLElement) => {
            const root = createRoot(host);
            flushSync(() => root.render(
              <BranchFamilyPanel
                projectId={projectId}
                projectPath={projectPath}
                sessionId={session.sessionId}
              />,
            ));
            return () => root.unmount();
          },
        },
      ] : []),
      ...(identityMissing ? [
        { separator: true as const },
        { label: t('paneGroup.forkNeedsIdentity'), disabled: true },
      ] : []),
      { separator: true },
      { label: t('paneGroup.closeTab'), onClick: () => void closePane(projectId, paneId) },
      { label: t('paneGroup.closePane'), shortcut: hotkeyLabel('closePane'), danger: true, onClick: () => void closeLeaf(projectId, paneId) },
    ]);
  }, [projectId, projectPath, t, node.panes]);

  if (!activePane) return null;

  const ctrlBtn =
    'w-6 h-6 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] opacity-60 hover:opacity-100 hover:text-[var(--accent)] hover:bg-[var(--border-subtle)] transition-all';

  const overlayRect: Record<DropZone, string> = {
    center: 'inset-0',
    left: 'top-0 bottom-0 left-0 right-1/2',
    right: 'top-0 bottom-0 left-1/2 right-0',
    top: 'top-0 left-0 right-0 bottom-1/2',
    bottom: 'top-1/2 left-0 right-0 bottom-0',
  };

  return (
    // pane-enter：新分出来的格子淡入并放大到位。项目切到后台是 display:none
    // 留着不卸载，不会重播；只有真正新建/重排分屏时这层才重挂载
    <div className="w-full h-full flex flex-col pane-enter" data-pty-id={activePane.ptyId}>
      {/* Tab bar */}
      <div
        data-panel-header
        className={`relative flex items-stretch bg-[var(--bg-elevated)] border-b text-xs overflow-x-auto select-none shrink-0 ${
          tabDrop
            ? 'border-[var(--accent)] bg-[var(--border-subtle)]'
            : 'border-[var(--border-subtle)]'
        }`}
        role="tablist"
        aria-label={t('paneGroup.tablistLabel')}
        onMouseMove={handleTabBarDragMove}
        onMouseUp={handleTabBarDrop}
        onMouseLeave={() => setTabDrop(null)}
        onDoubleClick={(e) => {
          // 只认空白区：tab 双击是重命名，按钮双击不该误触
          if ((e.target as Element).closest('[data-pane-tab],button')) return;
          toggleMaximize();
        }}
      >
        {node.panes.map((pane) => {
          const isActive = pane.id === activePane.id;
          // 会话跑的是哪家 AI 就亮哪家品牌图标:hook 上报的 agent 权威,
          // 输入检测的 detectedAgent 兜底;亮不亮的判定见 paneShowsAiSession
          // (含重启后待续接、尚未激活恢复的 pane —— 图标不该等切过去才出现)
          const aiActive = paneShowsAiSession(pane, autoResumeEnabled);
          return (
            <div
              key={pane.id}
              data-pane-tab
              role="tab"
              onMouseDown={(e) => {
                // 左键、且不落在关闭按钮上才启动拖拽跟踪；未越过 5px 阈值仍是普通点击
                if (e.button !== 0) return;
                if ((e.target as Element).closest('button')) return;
                closeTabPreview();
                initPaneDrag(
                  { projectId, paneId: pane.id },
                  e.currentTarget as HTMLElement,
                  e.clientX,
                  e.clientY,
                );
              }}
              tabIndex={isActive ? 0 : -1}
              aria-selected={isActive}
              // 状态点 + 标题 + 关闭按钮作为一组在 tab 内居中：左右 padding 对称，
              // 再给一个最小宽度 —— 否则 tab 宽度完全由内容撑开，居中无从谈起，
              // 短标题（nushell）和长标题的 tab 也会宽窄不一显得毛糙。
              className={`flex items-center justify-center gap-1.5 px-3 py-[3px] min-w-[7.5rem] cursor-pointer whitespace-nowrap transition-all duration-100 relative ${
                isActive
                  ? 'bg-[var(--bg-terminal)] text-[var(--text-primary)]'
                  : 'text-[var(--text-muted)] hover:text-[var(--text-secondary)] hover:bg-[var(--border-subtle)]'
              }`}
              onClick={() => {
                closeTabPreview();
                activatePane(projectId, pane.id);
              }}
              onDoubleClick={() => void renamePane(projectId, pane.id)}
              onMouseEnter={(e) => {
                // 激活 tab 的画面就在眼前,只有隐藏 tab 需要预览
                if (!isActive) handleTabPreviewEnter(e, pane.id);
              }}
              onMouseLeave={closeTabPreview}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  activatePane(projectId, pane.id);
                }
              }}
              onContextMenu={(e) => {
                closeTabPreview();
                paneContextMenu(e, pane.id);
              }}
            >
              {/* 激活指示条:始终占位、未激活透明(同 SettingsModal tab 惯例)。
                  背景图皮肤下 --bg-terminal 与 --bg-elevated 几乎同色,
                  仅靠底色分不出激活 tab,accent 条不吃主题 */}
              <span
                aria-hidden
                className={`absolute top-0 left-0 right-0 h-[2px] ${isActive ? 'bg-[var(--accent)]' : 'bg-transparent'}`}
              />
              <StatusDot status={pane.status} />
              {aiActive && (
                <BrandIcon
                  vendor={inferVendor({ agent: pane.aiSession?.agent ?? pane.detectedAgent })}
                  size={12}
                />
              )}
              <span className="font-medium">{paneLabel(pane)}</span>
              <button
                type="button"
                className="ml-0.5 w-4 h-4 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--color-error)] hover:bg-[var(--border-subtle)] transition-colors"
                title={t('paneGroup.closeTab')}
                aria-label={t('paneGroup.closeTabLabel', { label: paneLabel(pane) })}
                onClick={(e) => {
                  e.stopPropagation();
                  void closePane(projectId, pane.id);
                }}
              >
                <svg width="9" height="9" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round">
                  <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
                </svg>
              </button>
            </div>
          );
        })}

        {/* 新建终端 */}
        <button
          type="button"
          className="px-2 text-[var(--text-muted)] hover:text-[var(--accent)] transition-colors"
          onClick={handleNewTabClick}
          title={`${t('terminalArea.newTerminal')} (${hotkeyLabel('newTerminal')})`}
          aria-label={t('terminalArea.newTerminal')}
        >
          <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round">
            <path d="M8 3.5v9M3.5 8h9" />
          </svg>
        </button>

        {/* 右侧控件：常驻低透明度（原先 opacity:0 全靠 hover，分屏功能基本无从发现） */}
        <div className="ml-auto flex items-center gap-0.5 px-1.5">
          {activePane.ptyId !== undefined && markers.length > 0 && (
            <button
              ref={markerBtnRef}
              type="button"
              className="mr-1 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--accent)] hover:bg-[var(--border-subtle)] flex items-center gap-1 transition-colors"
              onClick={() => (markerOpen ? setMarkerOpen(false) : openMarkerPopover())}
              title={t('paneGroup.markerTooltip', { mod: MOD_LABEL })}
              aria-expanded={markerOpen}
            >
              <span>⚑</span>
              <span className="tabular-nums">{markers.length}</span>
            </button>
          )}
          {activePane.ptyId !== undefined && (
            <button
              type="button"
              className={ctrlBtn}
              title={`${t('terminalSearch.title')} (${hotkeyLabel('terminalSearch')})`}
              aria-label={t('terminalSearch.title')}
              onClick={() => openTerminalSearch(activePane.ptyId!)}
            >
              {ICON_SEARCH}
            </button>
          )}
          {(layoutIsSplit || isMaximized) && (
            <button
              type="button"
              className={ctrlBtn}
              title={isMaximized ? t('paneGroup.restorePane') : t('paneGroup.maximizePane')}
              aria-label={isMaximized ? t('paneGroup.restorePane') : t('paneGroup.maximizePane')}
              onClick={toggleMaximize}
            >
              {isMaximized ? ICON_RESTORE : ICON_MAXIMIZE}
            </button>
          )}
          <button
            type="button"
            className={ctrlBtn}
            title={`${t('paneGroup.splitRight')} (${hotkeyLabel('splitRight')})`}
            aria-label={t('paneGroup.splitRight')}
            onClick={() => void splitPane(projectId, 'horizontal', activePane.id)}
          >
            {ICON_SPLIT_RIGHT}
          </button>
          <button
            type="button"
            className={ctrlBtn}
            title={`${t('paneGroup.splitDown')} (${hotkeyLabel('splitDown')})`}
            aria-label={t('paneGroup.splitDown')}
            onClick={() => void splitPane(projectId, 'vertical', activePane.id)}
          >
            {ICON_SPLIT_DOWN}
          </button>
          <button
            type="button"
            className={`${ctrlBtn} hover:!text-[var(--color-error)]`}
            title={`${t('paneGroup.closePane')} (${hotkeyLabel('closePane')})`}
            aria-label={t('paneGroup.closePane')}
            onClick={() => void closeLeaf(projectId, activePane.id)}
          >
            {ICON_CLOSE}
          </button>
        </div>

        {/* 插入位指示线：随内容坐标定位，跟着 tab 栏横向滚动 */}
        {tabDrop && (
          <span
            aria-hidden
            className="absolute top-[2px] bottom-[2px] w-[3px] rounded-full bg-[var(--accent)] pointer-events-none z-10"
            style={{
              left: Math.max(tabDrop.x - 1.5, 0),
              boxShadow: '0 0 6px var(--accent), 0 0 2px var(--accent)',
            }}
          />
        )}
      </div>

      {/* Active terminal */}
      <div
        className="flex-1 overflow-hidden relative"
        onMouseMove={handleBodyDragMove}
        onMouseUp={handleBodyDrop}
        onMouseLeave={() => setDropZone(null)}
      >
        <div className="absolute inset-0">
          {activePane.ptyId !== undefined ? (
            <>
              <TerminalInstance
                ptyId={activePane.ptyId}
              />
              {/* 远程断线覆盖层:保留 pane,点击在同一 pane 重连 */}
              {showReconnect && (
                <div className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-black/55 backdrop-blur-[1px]">
                  <div className="text-sm text-[var(--text-secondary)]">
                    {t('paneGroup.remoteDisconnected')}
                  </div>
                  <button
                    type="button"
                    className="px-3 py-1.5 text-sm rounded-[var(--radius-sm)] border border-[var(--border-default)] text-[var(--text-secondary)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-colors"
                    onClick={handleReconnect}
                  >
                    {t('paneGroup.reconnect')}
                  </button>
                </div>
              )}
            </>
          ) : activePane.status === 'error' ? (
            <div className="h-full flex flex-col items-center justify-center gap-2 text-[var(--text-muted)] text-sm px-4">
              <div>{t('paneGroup.startFailed')}</div>
              {spawnErrors[activePane.id] && (
                <div className="text-xs text-[var(--color-error)] max-w-[80%] text-center break-all">
                  {spawnErrors[activePane.id]}
                </div>
              )}
              <button
                type="button"
                className="px-3 py-1.5 rounded-[var(--radius-sm)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-colors"
                onClick={handleRetryCreatePty}
              >
                {t('paneGroup.retry')}
              </button>
            </div>
          ) : (
            <div className="h-full flex items-center justify-center text-[var(--text-muted)] text-sm">
              {t('paneGroup.starting')}
            </div>
          )}
        </div>
        {/* 拖拽落点预览：center 铺满 = 并入本组，半屏 = 往该方向分屏 */}
        {dropZone && (
          <div
            aria-hidden
            className={`absolute z-20 pointer-events-none border-2 border-[var(--accent)] bg-[var(--accent)]/20 ${overlayRect[dropZone]}`}
          />
        )}
      </div>

      {activePane.ptyId !== undefined && markerOpen && markerAnchor && createPortal(
        <div
          ref={markerPopoverRef}
          className="fixed z-50 rounded-md border shadow-lg"
          style={{
            top: markerAnchor.top,
            right: markerAnchor.right,
            background: 'var(--bg-elevated)',
            borderColor: 'var(--border-subtle)',
          }}
        >
          <MarkerList
            ptyId={activePane.ptyId}
            markers={markers}
            onClose={() => setMarkerOpen(false)}
          />
        </div>,
        document.body,
      )}

      {/* 悬停缩略图:渲染处每次重判 —— tab 被关闭/被激活那一帧就不画,不留旧锚点 */}
      {tabPreview && (() => {
        const previewPane = node.panes.find((p) => p.id === tabPreview.paneId);
        if (!previewPane || previewPane.id === activePane.id) return null;
        return createPortal(
          <PaneTabPreview pane={previewPane} anchorRect={tabPreview.rect} />,
          document.body,
        );
      })()}
    </div>
  );
}

/**
 * 全局快捷键派发。键位表见 `utils/hotkeys.ts`（唯一事实来源）。
 *
 * 三条边界：
 *  - 弹窗打开时不派发终端类动作（用户在填表，Ctrl+Shift+W 不该去关终端）
 *  - 焦点在输入框 / textarea 里时同样放行给输入框
 *  - AI 标记跳转由 `useMarkerHotkeys` 单独处理（它需要维护「上次跳到哪」的游标）
 */
import { useEffect } from 'react';
import { useAppStore } from '../store';
import { resolveHotkey } from '../utils/hotkeys';
import {
  closeLeaf,
  cyclePane,
  focusAdjacentPane,
  newTerminal,
  renamePane,
  selectPaneByIndex,
  splitPane,
} from '../utils/paneActions';
import { resolveActivePane } from '../utils/layoutOps';
import { openTerminalSearch } from '../utils/terminalSearch';
import { isOverlayOpen } from '../utils/overlayStack';
import { isMac } from '../utils/platform';

/** marker 跳转归 useMarkerHotkeys 管，这里不重复处理 */
const HANDLED_ELSEWHERE = new Set(['markerPrev', 'markerNext']);

/**
 * 拦下这次按键：preventDefault **不够**。
 *
 * xterm 的 keydown handler 不看 `defaultPrevented`，只看 customKeyEventHandler
 * 的返回值。光 preventDefault 的话，事件继续 capture 到 helper textarea，
 * xterm 照常把它翻成控制序列送进 PTY —— Ctrl+F 会在打开查找条的同时往 shell
 * 发 \x06，F2 发 \x1bOQ（htop 会打开设置界面），Ctrl+Tab 发 \t（触发补全）。
 * 必须 stopPropagation 把事件截在 window 这一层。
 */
function consume(e: KeyboardEvent): void {
  e.preventDefault();
  e.stopPropagation();
}

/** 需要「当前有活动项目」才有意义的动作 */
const NEEDS_PROJECT = new Set([
  'newTerminal', 'closePane', 'renamePane', 'splitRight', 'splitDown',
  'nextPane', 'prevPane', 'selectPaneN',
  'focusLeft', 'focusRight', 'focusUp', 'focusDown',
  'terminalSearch',
]);

function isTypingTarget(el: EventTarget | null): boolean {
  if (!(el instanceof HTMLElement)) return false;
  // 富文本编辑区也算「在打字」，应用级快捷键让路
  if (el.isContentEditable) return true;
  const tag = el.tagName;
  if (tag !== 'INPUT' && tag !== 'TEXTAREA') return false;
  // xterm 的 helper textarea 带 class="xterm-helper-textarea"，那正是终端输入本身
  return !el.classList.contains('xterm-helper-textarea');
}

interface Options {
  onOpenSettings: () => void;
  onSwitchProject: () => void;
}

export function useGlobalHotkeys({ onOpenSettings, onSwitchProject }: Options): void {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      // 「有没有覆盖物压着」以 overlayStack 为准,而不是 App 手工维护的布尔:
      // 手工列表漏掉了文件预览/差异/会话查看器/命令式确认框等一大半,
      // 结果查看器开着时 Ctrl+Shift+W 会去关它后面的终端。
      const overlayOpen = isOverlayOpen();
      const state = useAppStore.getState();
      const projectId = state.activeProjectId;

      // Ctrl+1..9 单独判：键位表里存的是占位串 '1…9'
      // 修饰键口径与 matchHotkey 对齐(mac 上认 ⌘ 不认 Ctrl)
      const mod = isMac ? e.metaKey : e.ctrlKey;
      const otherMod = isMac ? e.ctrlKey : e.metaKey;
      if (mod && !otherMod && !e.shiftKey && !e.altKey && /^[1-9]$/.test(e.key)) {
        if (overlayOpen || !projectId || isTypingTarget(e.target)) return;
        consume(e);
        selectPaneByIndex(projectId, Number(e.key));
        return;
      }

      const id = resolveHotkey(e);
      if (!id || HANDLED_ELSEWHERE.has(id)) return;
      if (isTypingTarget(e.target)) return;
      // globalSearch 要放行:它是 toggle,弹窗开着时按第二次才能关掉
      if (overlayOpen && id !== 'openSettings' && id !== 'globalSearch') return;
      if (NEEDS_PROJECT.has(id) && !projectId) return;

      const layout = projectId ? state.projectStates.get(projectId)?.layout ?? null : null;

      switch (id) {
        case 'newTerminal':
          consume(e);
          void newTerminal(projectId!);
          break;
        case 'closePane': {
          consume(e);
          const active = resolveActivePane(layout);
          if (active) void closeLeaf(projectId!, active.id);
          break;
        }
        case 'renamePane':
          consume(e);
          void renamePane(projectId!);
          break;
        case 'splitRight':
          consume(e);
          void splitPane(projectId!, 'horizontal');
          break;
        case 'splitDown':
          consume(e);
          void splitPane(projectId!, 'vertical');
          break;
        case 'nextPane':
          consume(e);
          cyclePane(projectId!, 1);
          break;
        case 'prevPane':
          consume(e);
          cyclePane(projectId!, -1);
          break;
        case 'focusLeft':
          consume(e);
          focusAdjacentPane(projectId!, 'left');
          break;
        case 'focusRight':
          consume(e);
          focusAdjacentPane(projectId!, 'right');
          break;
        case 'focusUp':
          consume(e);
          focusAdjacentPane(projectId!, 'up');
          break;
        case 'focusDown':
          consume(e);
          focusAdjacentPane(projectId!, 'down');
          break;
        case 'terminalSearch': {
          const active = resolveActivePane(layout);
          if (active?.ptyId === undefined) return; // 没终端就别拦 Ctrl+F
          consume(e);
          openTerminalSearch(active.ptyId);
          break;
        }
        case 'globalSearch': {
          // 内容搜索是本地 ripgrep 链路，远程项目不支持
          const project = state.config.projects.find((p) => p.id === projectId);
          if (!state.searchModalOpen && project?.sshConnectionId) return;
          consume(e);
          state.setSearchModalOpen(!state.searchModalOpen);
          break;
        }
        case 'switchProject':
          consume(e);
          onSwitchProject();
          break;
        case 'openSettings':
          consume(e);
          onOpenSettings();
          break;
        case 'toggleSidebar':
          consume(e);
          state.toggleMiddleColumn();
          break;
      }
    };

    // capture 阶段：xterm 的 keydown 监听在 textarea 上，冒泡阶段抢不过它
    window.addEventListener('keydown', handler, { capture: true });
    return () => window.removeEventListener('keydown', handler, { capture: true });
  }, [onOpenSettings, onSwitchProject]);
}

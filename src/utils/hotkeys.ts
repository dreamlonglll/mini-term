/**
 * 全应用快捷键的**唯一事实来源**。
 *
 * 这张表同时被三处消费：
 *   1. `useGlobalHotkeys` —— 实际的键盘监听与派发
 *   2. 设置 →「快捷键」页 —— 按 group 分组渲染（不再手写静态说明，改不动就漂移）
 *   3. 空态 / tooltip 里的 `hotkeyLabel(id)` —— 显示串
 *
 * ## 键位选择原则
 *
 * 终端应用的第一约束是**不能吞掉 shell / TUI 需要的按键**。裸 `Ctrl+T`（bash
 * transpose-chars）、`Ctrl+W`（删除前一个词）、`Ctrl+P`（上一条历史）都有既定语义，
 * 因此应用级动作统一走 `Ctrl+Shift+*`（Windows Terminal / VS Code 终端的惯例），
 * 只有确实不与行编辑冲突的才用裸 `Ctrl`（`Ctrl+Tab`、`Ctrl+1..9`、`Ctrl+,`）。
 */
import { isMac } from './platform';

/** 快捷键作用域：决定按键在什么情况下被拦截。 */
export type HotkeyScope =
  /** 全局：任何时候（弹窗打开时除外，见 useGlobalHotkeys） */
  | 'global'
  /** 终端内：由 xterm 的 customKeyEventHandler 处理，此处仅用于设置页展示 */
  | 'terminal';

export interface HotkeyDef {
  id: string;
  /** 按键组合描述；`key` 比较 e.key（大小写不敏感），`code` 比较 e.code */
  combo: {
    mod?: boolean;     // Ctrl（mac 上是 ⌘）
    shift?: boolean;
    alt?: boolean;
    key?: string;
    code?: string;
  };
  scope: HotkeyScope;
  /** 设置页分组的 i18n key */
  groupKey: string;
  /** 动作描述的 i18n key */
  descKey: string;
}

const G_TERMINAL = 'settings.shortcuts.terminalOps';
const G_NAV = 'settings.shortcuts.navigation';
const G_GLOBAL = 'settings.shortcuts.global';
const G_MARKER = 'settings.shortcuts.aiTaskMarks';
const G_CLIPBOARD = 'settings.shortcuts.clipboard';

export const HOTKEYS: HotkeyDef[] = [
  // ── 终端操作 ──
  { id: 'newTerminal', combo: { mod: true, shift: true, code: 'KeyT' }, scope: 'global', groupKey: G_TERMINAL, descKey: 'settings.shortcuts.newTerminal' },
  { id: 'closePane', combo: { mod: true, shift: true, code: 'KeyW' }, scope: 'global', groupKey: G_TERMINAL, descKey: 'settings.shortcuts.closePane' },
  { id: 'renamePane', combo: { key: 'F2' }, scope: 'global', groupKey: G_TERMINAL, descKey: 'settings.shortcuts.renamePane' },
  { id: 'splitRight', combo: { mod: true, shift: true, code: 'KeyD' }, scope: 'global', groupKey: G_TERMINAL, descKey: 'settings.shortcuts.splitRight' },
  { id: 'splitDown', combo: { mod: true, shift: true, code: 'KeyE' }, scope: 'global', groupKey: G_TERMINAL, descKey: 'settings.shortcuts.splitDown' },

  // ── 导航 ──
  { id: 'nextPane', combo: { mod: true, key: 'Tab' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.nextPane' },
  { id: 'prevPane', combo: { mod: true, shift: true, key: 'Tab' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.prevPane' },
  { id: 'selectPaneN', combo: { mod: true, key: '1…9' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.selectPaneN' },
  { id: 'focusLeft', combo: { alt: true, key: 'ArrowLeft' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.focusLeft' },
  { id: 'focusRight', combo: { alt: true, key: 'ArrowRight' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.focusRight' },
  { id: 'focusUp', combo: { alt: true, key: 'ArrowUp' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.focusUp' },
  { id: 'focusDown', combo: { alt: true, key: 'ArrowDown' }, scope: 'global', groupKey: G_NAV, descKey: 'settings.shortcuts.focusDown' },

  // ── 全局 ──
  { id: 'switchProject', combo: { mod: true, shift: true, code: 'KeyP' }, scope: 'global', groupKey: G_GLOBAL, descKey: 'settings.shortcuts.switchProject' },
  { id: 'globalSearch', combo: { mod: true, shift: true, code: 'KeyF' }, scope: 'global', groupKey: G_GLOBAL, descKey: 'settings.shortcuts.toggleGlobalSearch' },
  { id: 'terminalSearch', combo: { mod: true, code: 'KeyF' }, scope: 'global', groupKey: G_GLOBAL, descKey: 'settings.shortcuts.terminalSearch' },
  { id: 'openSettings', combo: { mod: true, key: ',' }, scope: 'global', groupKey: G_GLOBAL, descKey: 'settings.shortcuts.openSettings' },
  { id: 'toggleSidebar', combo: { mod: true, shift: true, code: 'KeyB' }, scope: 'global', groupKey: G_GLOBAL, descKey: 'settings.shortcuts.toggleSidebar' },

  // ── AI 任务标记 ──
  { id: 'markerPrev', combo: { mod: true, shift: true, key: 'ArrowUp' }, scope: 'global', groupKey: G_MARKER, descKey: 'settings.shortcuts.jumpPrevAi' },
  { id: 'markerNext', combo: { mod: true, shift: true, key: 'ArrowDown' }, scope: 'global', groupKey: G_MARKER, descKey: 'settings.shortcuts.jumpNextAi' },

  // ── 剪贴板（终端内，由 xterm 的 key handler 消费；此处仅供设置页展示）──
  { id: 'copySelection', combo: { mod: true, shift: true, code: 'KeyC' }, scope: 'terminal', groupKey: G_CLIPBOARD, descKey: 'settings.shortcuts.copySelected' },
  { id: 'pasteToTerminal', combo: { mod: true, shift: true, code: 'KeyV' }, scope: 'terminal', groupKey: G_CLIPBOARD, descKey: 'settings.shortcuts.pasteToTerminal' },
];

const BY_ID = new Map(HOTKEYS.map((h) => [h.id, h]));

/** 该按键事件是否命中给定快捷键。 */
export function matchHotkey(e: KeyboardEvent, def: HotkeyDef): boolean {
  const { combo } = def;
  const mod = isMac ? e.metaKey : e.ctrlKey;
  // 平台交叉键不算命中：Windows 上按 ⌘（Meta）不该触发 Ctrl 绑定，反之亦然
  const otherMod = isMac ? e.ctrlKey : e.metaKey;
  if (otherMod) return false;
  if (!!combo.mod !== mod) return false;
  if (!!combo.shift !== e.shiftKey) return false;
  if (!!combo.alt !== e.altKey) return false;
  // code 优先：布局无关（Shift+D 在 e.key 上是 'D'，在非 QWERTY 布局上更不可靠）。
  // 但 e.code 并非总是可用 —— 合成输入（自动化 / 部分远程桌面客户端 / 某些 IME
  // 链路）给出的 KeyboardEvent 可能带空 code，此时退回比较 e.key，不然快捷键
  // 会在这些环境里整体失灵。
  if (combo.code) {
    if (e.code === combo.code) return true;
    if (e.code) return false;
    const letter = combo.code.replace(/^Key/, '');
    return letter.length === 1 && e.key.toLowerCase() === letter.toLowerCase();
  }
  if (combo.key) return e.key.toLowerCase() === combo.key.toLowerCase();
  return false;
}

/** 找出该按键事件命中的快捷键 id（global 作用域内），没有则 null。 */
export function resolveHotkey(e: KeyboardEvent): string | null {
  for (const def of HOTKEYS) {
    if (def.scope !== 'global') continue;
    if (matchHotkey(e, def)) return def.id;
  }
  return null;
}

const KEY_LABELS: Record<string, string> = {
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
  Tab: 'Tab',
  ',': ',',
};

/** 单条快捷键的显示串，如 `Ctrl+Shift+T` / `⌘⇧T`。 */
export function comboLabel(combo: HotkeyDef['combo']): string {
  const parts: string[] = [];
  if (combo.mod) parts.push(isMac ? '⌘' : 'Ctrl');
  if (combo.shift) parts.push(isMac ? '⇧' : 'Shift');
  if (combo.alt) parts.push(isMac ? '⌥' : 'Alt');
  const raw = combo.code?.replace(/^Key/, '') ?? combo.key ?? '';
  parts.push(KEY_LABELS[raw] ?? raw);
  return parts.join(isMac ? '' : '+');
}

/** 按 id 取显示串；id 不存在时返回空串（调用点不会因此崩） */
export function hotkeyLabel(id: string): string {
  const def = BY_ID.get(id);
  return def ? comboLabel(def.combo) : '';
}

/** 设置页用：按 groupKey 归组，保持表内声明顺序。 */
export function hotkeyGroups(): { groupKey: string; items: HotkeyDef[] }[] {
  const groups: { groupKey: string; items: HotkeyDef[] }[] = [];
  for (const def of HOTKEYS) {
    let bucket = groups.find((g) => g.groupKey === def.groupKey);
    if (!bucket) {
      bucket = { groupKey: def.groupKey, items: [] };
      groups.push(bucket);
    }
    bucket.items.push(def);
  }
  return groups;
}

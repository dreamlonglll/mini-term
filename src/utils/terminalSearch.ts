/**
 * 终端内查找（Ctrl+F）。
 *
 * scrollback 开到 10 万行却只能手滚，是这个应用之前最实在的缺口。
 * SearchAddon 按 ptyId 懒加载并挂在 terminalCache 的 entry 上，跟随终端实例的
 * 生命周期（终端 dispose 时 addon 一并释放，无需额外簿记）。
 */
import { create } from 'zustand';
import { SearchAddon } from '@xterm/addon-search';
import { getCachedTerminal, getSearchAddon, setSearchAddon } from './terminalCache';

export interface SearchState {
  /** 正在搜索哪个终端；null = 查找条关闭 */
  ptyId: number | null;
  query: string;
  caseSensitive: boolean;
  regex: boolean;
  wholeWord: boolean;
  /** addon 回报的命中位置；count<0 表示尚未搜索 */
  resultIndex: number;
  resultCount: number;
}

interface SearchStore extends SearchState {
  set: (patch: Partial<SearchState>) => void;
}

export const useTerminalSearchStore = create<SearchStore>((set) => ({
  ptyId: null,
  query: '',
  caseSensitive: false,
  regex: false,
  wholeWord: false,
  resultIndex: -1,
  resultCount: -1,
  set: (patch) => set(patch),
}));

/** 取（必要时创建）该终端的 SearchAddon。终端不存在时返回 null。 */
function ensureAddon(ptyId: number): SearchAddon | null {
  const cached = getCachedTerminal(ptyId);
  if (!cached) return null;
  const existing = getSearchAddon(ptyId);
  if (existing) return existing;

  const addon = new SearchAddon();
  cached.term.loadAddon(addon);
  addon.onDidChangeResults(({ resultIndex, resultCount }) => {
    // 只有当前正在搜的终端才回写，防止后台终端的迟到回调覆盖 UI
    if (useTerminalSearchStore.getState().ptyId !== ptyId) return;
    useTerminalSearchStore.getState().set({ resultIndex, resultCount });
  });
  setSearchAddon(ptyId, addon);
  return addon;
}

function searchOptions() {
  const { caseSensitive, regex, wholeWord } = useTerminalSearchStore.getState();
  return {
    caseSensitive,
    regex,
    wholeWord,
    decorations: {
      matchBackground: '#c8805a55',
      matchBorder: '#c8805a',
      matchOverviewRuler: '#c8805a',
      activeMatchBackground: '#c8805aaa',
      activeMatchBorder: '#f0ece6',
      activeMatchColorOverviewRuler: '#f0ece6',
    },
  };
}

export function openTerminalSearch(ptyId: number): void {
  const store = useTerminalSearchStore.getState();
  store.set({ ptyId, resultIndex: -1, resultCount: -1 });
  // 换了终端就重新搜一遍，沿用上次的关键词（连续排查同一个报错时很省事）
  if (store.query) requestAnimationFrame(() => findNext());
}

export function closeTerminalSearch(): void {
  const { ptyId } = useTerminalSearchStore.getState();
  if (ptyId != null) {
    ensureAddon(ptyId)?.clearDecorations();
    // 焦点还给终端：不还的话 activeElement 停在已卸载的输入框 → body，
    // 用户接着敲的字全部落空，还得先用鼠标点一下终端才能继续
    getCachedTerminal(ptyId)?.term.focus();
  }
  useTerminalSearchStore.getState().set({ ptyId: null, resultIndex: -1, resultCount: -1 });
}

/** 查找条正指着这个终端就关掉它（pane 被关 / 终端被销毁时调用）。 */
export function closeTerminalSearchFor(ptyId: number): void {
  if (useTerminalSearchStore.getState().ptyId !== ptyId) return;
  // 终端马上就要没了，不去碰 addon，直接收起 UI
  useTerminalSearchStore.getState().set({ ptyId: null, resultIndex: -1, resultCount: -1 });
}

/** 关键词或选项变化后重搜（从当前位置起找第一个命中）。 */
export function runSearch(): void {
  const { ptyId, query } = useTerminalSearchStore.getState();
  if (ptyId == null) return;
  const addon = ensureAddon(ptyId);
  if (!addon) return;
  if (!query) {
    addon.clearDecorations();
    useTerminalSearchStore.getState().set({ resultIndex: -1, resultCount: -1 });
    return;
  }
  addon.findNext(query, { ...searchOptions(), incremental: true });
}

export function findNext(): void {
  const { ptyId, query } = useTerminalSearchStore.getState();
  if (ptyId == null || !query) return;
  ensureAddon(ptyId)?.findNext(query, searchOptions());
}

export function findPrevious(): void {
  const { ptyId, query } = useTerminalSearchStore.getState();
  if (ptyId == null || !query) return;
  ensureAddon(ptyId)?.findPrevious(query, searchOptions());
}

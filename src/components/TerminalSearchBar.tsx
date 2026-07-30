import { useEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import {
  closeTerminalSearch,
  findNext,
  findPrevious,
  runSearch,
  useTerminalSearchStore,
} from '../utils/terminalSearch';
import { useT } from '../i18n';

/**
 * 终端内查找条：浮在当前 pane 的右上角。
 *
 * 全局单例（同一时刻只搜一个终端），位置跟随目标 pane 的 `[data-pty-id]` 容器，
 * 因此分屏、拖动分隔条、切 pane 都不需要额外的定位簿记。
 */
export function TerminalSearchBar() {
  const t = useT();
  const ptyId = useTerminalSearchStore((s) => s.ptyId);
  const query = useTerminalSearchStore((s) => s.query);
  const caseSensitive = useTerminalSearchStore((s) => s.caseSensitive);
  const regex = useTerminalSearchStore((s) => s.regex);
  const wholeWord = useTerminalSearchStore((s) => s.wholeWord);
  const resultIndex = useTerminalSearchStore((s) => s.resultIndex);
  const resultCount = useTerminalSearchStore((s) => s.resultCount);
  const patch = useTerminalSearchStore((s) => s.set);

  const inputRef = useRef<HTMLInputElement>(null);
  const boxRef = useRef<HTMLDivElement>(null);

  // 打开 / 切换目标终端时聚焦并全选，便于直接改关键词
  useEffect(() => {
    if (ptyId == null) return;
    const el = inputRef.current;
    if (!el) return;
    el.focus();
    el.select();
  }, [ptyId]);

  // 跟随目标 pane 定位：pane 可能被拖动/分屏改变位置，用 rAF 轮询比给每个
  // 布局变化点都挂回调可靠（这一条只在查找条打开时跑）
  useEffect(() => {
    if (ptyId == null) return;
    let raf = 0;
    const place = () => {
      const target = document.querySelector<HTMLElement>(
        `[data-terminal-drop][data-pty-id="${ptyId}"]`,
      );
      const box = boxRef.current;
      // 目标终端没了、或被切到后台项目（App 用 display:none 留着不卸载，
      // 元素还在但尺寸为 0）→ 直接收起查找条。否则它会顶着 0 宽高的矩形
      // 贴到屏幕左上角，看上去像是挂在**当前**项目的终端上，搜的却是另一个。
      const rect = target?.getBoundingClientRect();
      if (!target || !rect || rect.width === 0 || rect.height === 0) {
        closeTerminalSearch();
        return;
      }
      if (box) {
        box.style.top = `${rect.top + 6}px`;
        box.style.left = `${Math.max(8, rect.right - box.offsetWidth - 14)}px`;
        box.style.visibility = 'visible';
      }
      raf = requestAnimationFrame(place);
    };
    raf = requestAnimationFrame(place);
    return () => cancelAnimationFrame(raf);
  }, [ptyId]);

  if (ptyId == null) return null;

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closeTerminalSearch();
      return;
    }
    if (e.key === 'Enter') {
      e.preventDefault();
      e.stopPropagation();
      if (e.shiftKey) findPrevious();
      else findNext();
    }
  };

  const toggle = (key: 'caseSensitive' | 'regex' | 'wholeWord', value: boolean) => {
    patch({ [key]: value } as never);
    // 选项变了要按新规则重搜，否则计数还停在旧结果上
    requestAnimationFrame(runSearch);
  };

  const btn = (active: boolean) =>
    `px-1.5 py-0.5 rounded-[var(--radius-sm)] text-xs font-mono transition-colors ${
      active
        ? 'bg-[var(--accent)] text-[var(--bg-base)]'
        : 'text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
    }`;

  const nav =
    'w-6 h-6 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors disabled:opacity-35 disabled:hover:bg-transparent';

  const counter =
    resultCount < 0 ? '' : resultCount === 0 ? t('terminalSearch.noResults') : `${resultIndex + 1}/${resultCount}`;

  return createPortal(
    <div
      ref={boxRef}
      role="search"
      aria-label={t('terminalSearch.title')}
      className="fixed z-40 flex items-center gap-1 px-2 py-1.5 rounded-[var(--radius-md)] bg-[var(--bg-elevated)] border border-[var(--border-strong)] shadow-[var(--shadow-overlay)] overlay-menu"
      style={{ visibility: 'hidden' }}
      onKeyDown={onKeyDown}
    >
      <input
        ref={inputRef}
        type="text"
        value={query}
        spellCheck={false}
        placeholder={t('terminalSearch.placeholder')}
        aria-label={t('terminalSearch.placeholder')}
        onChange={(e) => {
          patch({ query: e.target.value });
          requestAnimationFrame(runSearch);
        }}
        className="w-48 bg-[var(--bg-base)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:outline-none focus:border-[var(--accent)]"
      />
      <span className="min-w-[3.5rem] text-center text-xs tabular-nums text-[var(--text-muted)]">
        {counter}
      </span>
      <button type="button" className={btn(caseSensitive)} title={t('terminalSearch.caseSensitive')}
        aria-pressed={caseSensitive} onClick={() => toggle('caseSensitive', !caseSensitive)}>Aa</button>
      <button type="button" className={btn(wholeWord)} title={t('terminalSearch.wholeWord')}
        aria-pressed={wholeWord} onClick={() => toggle('wholeWord', !wholeWord)}>ab</button>
      <button type="button" className={btn(regex)} title={t('terminalSearch.regex')}
        aria-pressed={regex} onClick={() => toggle('regex', !regex)}>.*</button>
      <button type="button" className={nav} title={t('terminalSearch.previous')}
        disabled={!query} onClick={findPrevious} aria-label={t('terminalSearch.previous')}>↑</button>
      <button type="button" className={nav} title={t('terminalSearch.next')}
        disabled={!query} onClick={findNext} aria-label={t('terminalSearch.next')}>↓</button>
      <button type="button" className={nav} title={t('terminalSearch.close')}
        onClick={closeTerminalSearch} aria-label={t('terminalSearch.close')}>✕</button>
    </div>,
    document.body,
  );
}

import { useEffect, useMemo, useRef, useState } from 'react';
import { useAppStore } from '../store';
import { Modal } from './Modal';
import { StatusDot } from './StatusDot';
import { getProjectsWithGroupPath } from '../utils/projectTree';
import { useT } from '../i18n';
import type { PaneStatus } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

/** 子序列模糊匹配（`mt` 命中 `mini-term`），返回命中位置用于高亮。 */
function fuzzyMatch(text: string, query: string): number[] | null {
  if (!query) return [];
  const lower = text.toLowerCase();
  const q = query.toLowerCase();
  const hits: number[] = [];
  let ti = 0;
  for (const ch of q) {
    const found = lower.indexOf(ch, ti);
    if (found < 0) return null;
    hits.push(found);
    ti = found + 1;
  }
  return hits;
}

function Highlight({ text, hits }: { text: string; hits: number[] }) {
  if (hits.length === 0) return <>{text}</>;
  const set = new Set(hits);
  return (
    <>
      {Array.from(text).map((ch, i) =>
        set.has(i)
          ? <span key={i} className="text-[var(--accent)] font-semibold">{ch}</span>
          : <span key={i}>{ch}</span>,
      )}
    </>
  );
}

/**
 * 项目快速切换器（Ctrl+Shift+P）。
 *
 * 之前切项目只能用鼠标点侧栏，侧栏还能被折叠起来 —— 折叠状态下键盘完全够不到项目。
 * 匹配同时吃项目名和分组路径，「前端/webapp」这类同名项目靠分组区分。
 */
export function ProjectSwitcher({ open, onClose }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const projectStates = useAppStore((s) => s.projectStates);
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const setActiveProject = useAppStore((s) => s.setActiveProject);

  const [query, setQuery] = useState('');
  const [cursor, setCursor] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  // 每次打开都从空查询开始：上次搜的词到下次几乎总是无关的
  useEffect(() => {
    if (open) {
      setQuery('');
      setCursor(0);
    }
  }, [open]);

  const items = useMemo(() => {
    const all = getProjectsWithGroupPath(config).map(({ project, groupPath }) => ({
      project,
      groupPath,
      status: (projectStates.get(project.id)?.status ?? 'idle') as PaneStatus,
      needsAttention: !!projectStates.get(project.id)?.needsAttention,
    }));
    if (!query.trim()) {
      // 无查询时：当前项目排最后（要切走的多半不是自己），其余保持侧栏顺序
      return all.map((it) => ({ ...it, hits: [] as number[] }));
    }
    const q = query.trim();
    return all.flatMap((it) => {
      const nameHits = fuzzyMatch(it.project.name, q);
      if (nameHits) return [{ ...it, hits: nameHits }];
      // 名字没中就试分组路径，命中时不高亮名字
      const path = it.groupPath.join('/');
      return fuzzyMatch(path, q) ? [{ ...it, hits: [] as number[] }] : [];
    });
  }, [config, projectStates, query]);

  useEffect(() => {
    setCursor((c) => Math.min(c, Math.max(0, items.length - 1)));
  }, [items.length]);

  // 键盘移动时把高亮项滚进可视区
  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>(`[data-idx="${cursor}"]`)
      ?.scrollIntoView({ block: 'nearest' });
  }, [cursor]);

  const commit = (index: number) => {
    const target = items[index];
    if (!target) return;
    setActiveProject(target.project.id);
    onClose();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setCursor((c) => (items.length === 0 ? 0 : (c + 1) % items.length));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setCursor((c) => (items.length === 0 ? 0 : (c - 1 + items.length) % items.length));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      commit(cursor);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      ariaLabel={t('projectSwitcher.title')}
      panelClassName="w-[460px] max-h-[60vh]"
      align="top"
    >
      <div className="px-3 py-2.5 border-b border-[var(--border-subtle)]" onKeyDown={onKeyDown}>
        <input
          type="text"
          value={query}
          spellCheck={false}
          placeholder={t('projectSwitcher.placeholder')}
          aria-label={t('projectSwitcher.placeholder')}
          onChange={(e) => { setQuery(e.target.value); setCursor(0); }}
          className="w-full bg-[var(--bg-base)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-3 py-2 text-sm text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:outline-none focus:border-[var(--accent)]"
        />
      </div>

      <div
        ref={listRef}
        className="flex-1 overflow-y-auto py-1"
        role="listbox"
        aria-label={t('projectSwitcher.title')}
        onKeyDown={onKeyDown}
      >
        {items.length === 0 ? (
          <div className="px-4 py-6 text-center text-sm text-[var(--text-muted)]">
            {t('projectSwitcher.noMatch')}
          </div>
        ) : items.map((item, idx) => {
          const isCursor = idx === cursor;
          const isActive = item.project.id === activeProjectId;
          return (
            <div
              key={item.project.id}
              data-idx={idx}
              role="option"
              aria-selected={isCursor}
              className={`flex items-center gap-2 px-3 py-2 mx-1 rounded-[var(--radius-sm)] cursor-pointer transition-colors ${
                isCursor ? 'bg-[var(--accent-subtle)]' : 'hover:bg-[var(--border-subtle)]'
              }`}
              onMouseEnter={() => setCursor(idx)}
              onClick={() => commit(idx)}
            >
              <div className="flex-1 min-w-0">
                <div className={`truncate text-sm ${isCursor ? 'text-[var(--accent)]' : 'text-[var(--text-primary)]'}`}>
                  <Highlight text={item.project.name} hits={item.hits} />
                  {isActive && (
                    <span className="ml-1.5 text-xs text-[var(--text-muted)]">
                      {t('projectSwitcher.current')}
                    </span>
                  )}
                </div>
                <div className="truncate text-xs text-[var(--text-muted)]">
                  {item.groupPath.length > 0 ? `${item.groupPath.join(' / ')} · ` : ''}
                  {item.project.path}
                </div>
              </div>
              {item.needsAttention && (
                <span className="text-xs text-[var(--color-success)] font-semibold">
                  {t('panels.done')}
                </span>
              )}
              {item.status !== 'idle' && <StatusDot status={item.status} />}
            </div>
          );
        })}
      </div>

      <div className="flex items-center gap-3 px-3 py-2 border-t border-[var(--border-subtle)] text-xs text-[var(--text-muted)] flex-shrink-0">
        <span><kbd className="kbd">↑</kbd><kbd className="kbd ml-0.5">↓</kbd> {t('projectSwitcher.hintMove')}</span>
        <span><kbd className="kbd">Enter</kbd> {t('projectSwitcher.hintOpen')}</span>
        <span><kbd className="kbd">Esc</kbd> {t('projectSwitcher.hintClose')}</span>
      </div>
    </Modal>
  );
}

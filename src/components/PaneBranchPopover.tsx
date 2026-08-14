import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import {
  buildSessionTree,
  findFamilyRoot,
  flattenSessionTree,
  mergeLineageEdges,
  type FlatSessionRow,
} from '../utils/sessionBranch';
import { findLiveSessionPane, jumpToSession } from '../utils/sessionJump';
import { forkPaneSession } from '../utils/paneActions';
import { BrandIcon } from './BrandIcon';
import { StatusDot } from './StatusDot';
import { useT } from '../i18n';
import type { AiVendor } from '../utils/inferVendor';
import type { AiSession, LineageEdge } from '../types';

/**
 * pane 右键「查看会话分支」的家族树浮层（设计: docs/plans/2026-08-14-session-branch-tree-design.md）。
 * 只画当前会话所在家族（从根到全部后代），标出「← 当前」；点击节点走
 * jumpToSession（已开切过去、未开新终端恢复），底部「再岔一条 ⇢ 新分屏」。
 * 数据与历史面板树视图同源：get_ai_sessions + scan_session_lineage + 自记账边。
 */

const CARD_WIDTH = 340;

const TYPE_VENDOR: Record<string, AiVendor> = { claude: 'claude', codex: 'openai', grok: 'grok' };

interface Props {
  projectId: string;
  projectPath: string;
  paneId: string;
  sessionId: string;
  anchor: { x: number; y: number };
  onClose: () => void;
}

export function PaneBranchPopover({ projectId, projectPath, paneId, sessionId, anchor, onClose }: Props) {
  const t = useT();
  const ref = useRef<HTMLDivElement>(null);
  const [rows, setRows] = useState<FlatSessionRow[] | null>(null);
  // 在跑徽章对 pane 状态保持反应性（订阅即可，行内直接查）
  useAppStore((s) => s.projectStates);

  useEffect(() => {
    let stale = false;
    Promise.all([
      invoke<AiSession[]>('get_ai_sessions', { projectPath }),
      invoke<LineageEdge[]>('scan_session_lineage', { projectPath }).catch(() => [] as LineageEdge[]),
    ])
      .then(([sessions, edges]) => {
        if (stale) return;
        const merged = mergeLineageEdges(
          edges,
          useAppStore.getState().config.sessionLineage ?? [],
        );
        const family = findFamilyRoot(buildSessionTree(sessions, merged), sessionId);
        setRows(family ? flattenSessionTree([family]) : []);
      })
      .catch(() => {
        if (!stale) setRows([]);
      });
    return () => {
      stale = true;
    };
  }, [projectPath, sessionId]);

  // 点外面 / Esc 关闭
  useEffect(() => {
    const onDoc = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [onClose]);

  const handleFork = useCallback(() => {
    onClose();
    void forkPaneSession(projectId, paneId);
  }, [onClose, projectId, paneId]);

  const left = Math.max(8, Math.min(anchor.x, window.innerWidth - CARD_WIDTH - 8));
  const top = Math.max(8, Math.min(anchor.y, window.innerHeight - 260));

  return (
    <div
      ref={ref}
      className="overlay-menu fixed z-50 rounded-md border text-xs"
      style={{
        left,
        top,
        width: CARD_WIDTH,
        background: 'var(--bg-overlay)',
        borderColor: 'var(--border-strong)',
        boxShadow: 'var(--shadow-overlay)',
        backdropFilter: 'blur(12px)',
      }}
    >
      <div className="px-2.5 pt-2 pb-1 text-[var(--text-muted)] uppercase tracking-[0.12em] text-[10px] font-medium">
        {t('paneGroup.branchPopover.title')}
      </div>
      <div className="max-h-[300px] overflow-y-auto px-1 pb-1">
        {rows === null && (
          <div className="px-2 py-3 text-center text-[var(--text-muted)]">{t('sessionList.loading')}</div>
        )}
        {rows !== null && rows.length === 0 && (
          <div className="px-2 py-3 text-center text-[var(--text-muted)]">
            {t('paneGroup.branchPopover.empty')}
          </div>
        )}
        {rows?.map(({ node, prefix }) => {
          const s = node.session;
          const isCurrent = s.id === sessionId;
          const live = findLiveSessionPane(s.id);
          return (
            <div
              key={s.id}
              className={`flex items-center gap-1.5 px-1.5 py-1 rounded-[var(--radius-sm)] cursor-pointer hover:bg-[var(--border-subtle)] transition-colors ${
                isCurrent ? 'bg-[var(--border-subtle)]' : ''
              }`}
              title={s.title}
              onClick={() => {
                onClose();
                if (!isCurrent) void jumpToSession(projectId, s);
              }}
            >
              {prefix && (
                <span className="flex-shrink-0 font-mono whitespace-pre text-[var(--text-muted)]">{prefix}</span>
              )}
              {live && <StatusDot status={live.status} />}
              <BrandIcon vendor={TYPE_VENDOR[s.sessionType] ?? 'claude'} size={12} />
              <span className="truncate text-[var(--text-secondary)]">{s.title}</span>
              {isCurrent && (
                <span className="ml-auto flex-shrink-0 text-[var(--accent)]">
                  {t('paneGroup.branchPopover.current')}
                </span>
              )}
            </div>
          );
        })}
      </div>
      <div className="border-t border-[var(--border-subtle)] p-1.5">
        <button
          type="button"
          className="w-full px-2 py-1.5 rounded-[var(--radius-sm)] border border-[var(--border-default)] text-[var(--text-secondary)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-colors"
          onClick={handleFork}
        >
          {t('paneGroup.branchPopover.forkAgain')}
        </button>
      </div>
    </div>
  );
}

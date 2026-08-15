import { useCallback, useEffect, useRef, useState } from 'react';
import { useAppStore } from '../store';
import { type FlatSessionRow } from '../utils/sessionBranch';
import { fetchFamilyRows, findLiveSessionPane, jumpToSession } from '../utils/sessionJump';
import { vendorForSession } from '../utils/inferVendor';
import { forkPaneSession } from '../utils/paneActions';
import { BrandIcon } from './BrandIcon';
import { StatusDot } from './StatusDot';
import { useT } from '../i18n';

/**
 * pane 右键「查看会话分支」的家族树浮层（设计: docs/plans/2026-08-14-session-branch-tree-design.md）。
 * 只画当前会话所在家族（从根到全部后代），标出「← 当前」；点击节点走
 * jumpToSession（已开切过去、未开新终端恢复），底部「再岔一条 ⇢ 新分屏」。
 *
 * 行标题用 LineageEdge.branchTitle（分叉后第一问）——fork 整份复制让标题
 * 继承根会话，分支之间全同名；行图标按**最新模型**推厂商（vendorForSession，
 * claude CLI 挂 GLM/DeepSeek 中转时按真实厂商亮 icon），pane tab 的 CLI 图标不受影响。
 */

const CARD_WIDTH = 340;

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
    fetchFamilyRows(projectPath, sessionId)
      .then((r) => {
        if (!stale) setRows(r);
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
          // 分支节点显示「分叉后第一问」,没有(分支还没提问)回落会话标题
          const displayTitle = node.edge?.branchTitle ?? s.title;
          return (
            <div
              key={s.id}
              className={`flex items-center gap-1.5 px-1.5 py-1 rounded-[var(--radius-sm)] cursor-pointer hover:bg-[var(--border-subtle)] transition-colors ${
                isCurrent ? 'bg-[var(--border-subtle)]' : ''
              }`}
              title={displayTitle}
              onClick={() => {
                onClose();
                if (!isCurrent) void jumpToSession(projectId, s);
              }}
            >
              {prefix && (
                <span className="flex-shrink-0 font-mono whitespace-pre text-[var(--text-muted)]">{prefix}</span>
              )}
              {live && <StatusDot status={live.status} />}
              <BrandIcon vendor={vendorForSession(s)} size={12} title={s.model ?? s.sessionType} />
              <span className="truncate text-[var(--text-secondary)]">{displayTitle}</span>
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

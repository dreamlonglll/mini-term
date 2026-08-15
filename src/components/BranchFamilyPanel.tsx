import { useEffect, useState } from 'react';
import { useAppStore } from '../store';
import { type FlatSessionRow } from '../utils/sessionBranch';
import { fetchFamilyRows, findLiveSessionPane, jumpToSession } from '../utils/sessionJump';
import { vendorForSession } from '../utils/inferVendor';
import { BrandIcon } from './BrandIcon';
import { StatusDot } from './StatusDot';
import { useT } from '../i18n';

/**
 * 「查看会话分支」悬停展开的家族树面板（设计: docs/plans/2026-08-14-session-branch-tree-design.md）。
 * 由 contextMenu 的 submenuRender 挂载:悬停展开/互斥/定位/随菜单关闭全部由
 * 菜单机制接管,本组件只管内容——连线、标题、图标、在跑状态与节点点击。
 *
 * 行标题用 LineageEdge.branchTitle(分叉后第一问)——fork 整份复制让标题继承
 * 根会话,分支之间全同名;行图标按**最新模型**推厂商(vendorForSession,claude
 * CLI 挂 GLM/DeepSeek 中转时按真实厂商亮 icon),pane tab 的 CLI 图标不受影响。
 *
 * 节点点击:jumpToSession(已开切过去、未开新终端恢复)。点击会冒泡到 document
 * 的菜单关闭监听,整个菜单随之收起,无需在此显式关菜单。
 */

const CARD_WIDTH = 340;

interface Props {
  projectId: string;
  projectPath: string;
  /** 当前 pane 的会话(高亮「← 当前」,点击禁用) */
  sessionId: string;
}

export function BranchFamilyPanel({ projectId, projectPath, sessionId }: Props) {
  const t = useT();
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

  return (
    <div
      className="rounded-md border text-xs"
      style={{
        width: CARD_WIDTH,
        background: 'var(--bg-overlay)',
        borderColor: 'var(--border-strong)',
        boxShadow: 'var(--shadow-overlay)',
        backdropFilter: 'blur(12px)',
      }}
    >
      <div className="max-h-[300px] overflow-y-auto p-1">
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
              className={`flex items-center gap-1.5 px-1.5 py-1 rounded-[var(--radius-sm)] transition-colors ${
                isCurrent
                  ? 'bg-[var(--border-subtle)] cursor-default'
                  : 'cursor-pointer hover:bg-[var(--border-subtle)]'
              }`}
              title={displayTitle}
              onClick={() => {
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
    </div>
  );
}

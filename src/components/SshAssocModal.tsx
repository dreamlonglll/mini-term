import { useState, useEffect, useMemo, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, saveConfigToDisk } from '../store';
import { showAlert } from '../utils/prompt';
import { buildGroupBuckets, connectionSummary, GroupSidebarRow } from './SshModal';
import type { SshGroupBucket } from './SshModal';
import { Modal, ModalCloseButton } from './Modal';
import { useT, t as tStatic } from '../i18n';
import type { EnableSshToolsResult, ProjectConfig } from '../types';

interface Props {
  /** 目标项目；null 表示弹窗关闭 */
  project: ProjectConfig | null;
  onClose: () => void;
}

/** 计算弹窗打开时的初始勾选集合。 */
function initialChecked(project: ProjectConfig, allIds: string[]): Set<string> {
  // 未启用 SSH 工具 → 默认全选（保存即以全部范围启用）
  if (!project.sshMcpEnabled) return new Set(allIds);
  // 已启用且设过范围 → 取已存范围（过滤掉已删除连接的陈旧 id）
  if (project.sshConnectionIds) {
    return new Set(project.sshConnectionIds.filter((id) => allIds.includes(id)));
  }
  // 已启用但未设范围 → 全部
  return new Set(allIds);
}

/** 两个范围是否等价。`undefined` 视为 allIds（兼容旧配置）。 */
function sameScope(a: string[] | undefined, b: string[], allIds: string[]): boolean {
  const effectiveA = a ?? allIds;
  if (effectiveA.length !== b.length) return false;
  const sb = new Set(b);
  return effectiveA.every((id) => sb.has(id));
}

/**
 * 「关联 SSH」弹窗：按项目设定 agent 可访问的 SSH 连接范围。
 *
 * 勾选 ≥1 个连接 = 为该项目启用 SSH 工具（CLI + Skill）并限定范围；全部取消 = 停用。
 * 范围始终存为显式 id 列表，新增连接不会被自动纳入已有项目。
 */
export function SshAssocModal({ project, onClose }: Props) {
  const t = useT();
  const connections = useAppStore((s) => s.config.sshConnections) ?? [];
  const sshGroups = useAppStore((s) => s.config.sshGroups) ?? [];
  const allIds = useMemo(() => connections.map((c) => c.id), [connections]);
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  /** 左栏选中态：null = 全部；'' = 未分组；其他 = 具名分组名（与 SshModal 一致） */
  const [selectedGroup, setSelectedGroup] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  /** 最后一次非空的目标项目，供关闭动画期间继续渲染 */
  const lastProjectRef = useRef<ProjectConfig | null>(project);

  useEffect(() => {
    if (project) {
      setChecked(initialChecked(project, allIds));
      setBusy(false);
      setSelectedGroup(null);
      setCollapsed(new Set());
    }
  }, [project, allIds]);

  const toggle = useCallback((id: string) => {
    setChecked((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const handleSave = useCallback(async () => {
    if (!project) return;
    const wasEnabled = !!project.sshMcpEnabled;
    const nowEnabled = checked.size > 0;
    // 始终存显式 id 列表，不用 undefined 表示"全选"
    const scope = allIds.filter((id) => checked.has(id));

    // 旧配置 sshConnectionIds === undefined 的语义是「含未来新增连接」，与显式 id
    // 列表并不等价：即便当前全选，若不落盘迁移成显式列表，之后新增 SSH 连接会被静默
    // 纳入该项目的可见范围（违背 v0.6.3「新增连接不自动纳入已有项目」的承诺）。
    // 故启用状态下 undefined 必须迁移；仅当迁移前后「当前有效范围」不变时静默落盘、
    // 不打扰用户（沿用 c25d99d 无改动不弹提示的意图）。
    const effectiveUnchanged =
      wasEnabled === nowEnabled &&
      (!nowEnabled || sameScope(project.sshConnectionIds, scope, allIds));

    // 未启用且仍未启用 → 没有生成物需要 reconcile，直接关闭。
    if (effectiveUnchanged && !nowEnabled) {
      onClose();
      return;
    }
    // 已启用项目每次保存都幂等 reconcile SKILL.md / 旧 MCP；有效配置没变时静默。
    // 这同时覆盖旧项目的 scope/token 迁移。
    const silentReconcile = effectiveUnchanged;

    setBusy(true);
    try {
      let projectToken = project.sshCliToken;
      if (nowEnabled) {
        const result = await invoke<EnableSshToolsResult>('enable_ssh_tools', {
          projectDir: project.path,
          projectToken,
        });
        projectToken = result.projectToken;
      } else if (!nowEnabled && wasEnabled) {
        await invoke('disable_ssh_tools', { projectDir: project.path });
      }

      const cfg = useAppStore.getState().config;
      const newConfig = {
        ...cfg,
        projects: cfg.projects.map((p) =>
          p.id === project.id
            ? {
                ...p,
                sshMcpEnabled: nowEnabled,
                sshCliToken: nowEnabled ? projectToken : undefined,
                sshConnectionIds: nowEnabled ? scope : undefined,
              }
            : p,
        ),
      };
      useAppStore.getState().setConfig(newConfig);
      await saveConfigToDisk(newConfig);
      onClose();

      // 幂等 reconcile / 存量迁移：落盘即可，不弹提示。
      if (silentReconcile) return;

      const scopeDesc =
        scope.length === allIds.length
          ? tStatic('sshAssoc.scopeAll', { count: allIds.length })
          : tStatic('sshAssoc.scopeSubset', { count: scope.length });
      if (nowEnabled && !wasEnabled) {
        await showAlert(
          tStatic('sshAssoc.enabledTitle'),
          tStatic('sshAssoc.enabledMessage', { name: project.name, scope: scopeDesc }),
        );
      } else if (nowEnabled && wasEnabled) {
        await showAlert(
          tStatic('sshAssoc.updatedTitle'),
          tStatic('sshAssoc.updatedMessage', { name: project.name, scope: scopeDesc }),
        );
      } else {
        await showAlert(
          tStatic('sshAssoc.disabledTitle'),
          tStatic('sshAssoc.disabledMessage', { name: project.name, scope: scopeDesc }),
        );
      }
    } catch (e: unknown) {
      setBusy(false);
      await showAlert(tStatic('sshAssoc.saveFailedTitle'), e instanceof Error ? e.message : String(e));
    }
  }, [project, checked, allIds, onClose]);

  // 关闭时父组件把 project 置了 null，但 Modal 还要播 ~0.14s 退场动画：
  // 这段时间照旧用最后一次的 project 渲染（Modal 内部也冻结了内容快照，
  // 这里只是保证 render 不去碰 null）
  if (project) lastProjectRef.current = project;
  const shown = project ?? lastProjectRef.current;
  if (!shown) return null;

  // 分组归类与 SshModal 共用同一份逻辑（含显式创建的空分组）
  const { namedGroups, ungroupedItems } = buildGroupBuckets(connections, sshGroups);
  const hasNamedGroup = namedGroups.length > 0;
  const groups: SshGroupBucket[] = [
    ...namedGroups,
    ...(ungroupedItems.length > 0 ? [{ group: undefined, items: ungroupedItems }] : []),
  ];
  const groupNames = namedGroups.map((g) => g.group);

  // 选中的分组可能因（在另一个弹窗里）删除/重命名而消失，回落到「全部」
  const activeGroup: string | null =
    selectedGroup === null
      ? null
      : selectedGroup === ''
        ? ungroupedItems.length > 0
          ? ''
          : null
        : groupNames.includes(selectedGroup)
          ? selectedGroup
          : null;

  // 右栏要展示的桶：全部视图展示所有桶（带可折叠标题），选中某组只展示该桶
  const visibleBuckets =
    activeGroup === null ? groups : groups.filter((g) => (g.group ?? '') === activeGroup);
  // 全选 / 全不选作用于「当前看得见的连接」：在某个分组里点全选，不该顺手把别的组也勾上
  const visibleIds = visibleBuckets.flatMap((b) => b.items.map((c) => c.id));

  const setCheckedFor = (ids: string[], on: boolean) => {
    setChecked((prev) => {
      const next = new Set(prev);
      for (const id of ids) {
        if (on) next.add(id);
        else next.delete(id);
      }
      return next;
    });
  };

  const toggleCollapsed = (key: string) => {
    setCollapsed((prev) => {
      const s = new Set(prev);
      if (s.has(key)) s.delete(key);
      else s.add(key);
      return s;
    });
  };

  return (
    <Modal
      open={!!project}
      onClose={onClose}
      ariaLabel={t('sshAssoc.title')}
      panelClassName="w-[720px] h-[70vh] max-h-[680px]"
      // 保存中不给关：正在写配置，半途退出会让 store 与磁盘不一致
      closeOnOverlay={!busy}
      closeOnEscape={!busy}
    >
      {/* 顶栏（带副标题，故不用 Modal 自带的 title） */}
      <div className="px-5 py-4 border-b border-[var(--border-subtle)] flex-shrink-0">
        <div className="flex items-center justify-between gap-3">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('sshAssoc.title')}</h2>
          <ModalCloseButton onClose={onClose} label={t('sshAssoc.cancel')} />
        </div>
        <div className="text-sm text-[var(--text-muted)] mt-1 truncate">
          {t('sshAssoc.subtitle', { name: shown.name })}
        </div>
      </div>

        {/* 内容：左侧分组列表 + 右侧连接列表（与「SSH 连接」弹窗同构） */}
        <div className="flex-1 flex min-h-0">
          {/* 左栏 */}
          <div className="w-44 flex-shrink-0 border-r border-[var(--border-subtle)] overflow-y-auto py-2 space-y-0.5">
            <GroupSidebarRow
              label={t('sshAssoc.allConnections')}
              count={connections.length}
              active={activeGroup === null}
              onClick={() => setSelectedGroup(null)}
            />
            {namedGroups.map((g) => (
              <GroupSidebarRow
                key={g.group}
                label={g.group}
                count={g.items.length}
                active={activeGroup === g.group}
                onClick={() => setSelectedGroup(g.group)}
              />
            ))}
            {ungroupedItems.length > 0 && (
              <GroupSidebarRow
                label={t('sshAssoc.ungrouped')}
                count={ungroupedItems.length}
                active={activeGroup === ''}
                onClick={() => setSelectedGroup('')}
              />
            )}
          </div>

          {/* 右栏 */}
          <div className="flex-1 min-w-0 overflow-y-auto px-5 py-4 space-y-3">
            {connections.length === 0 ? (
              <div className="text-center text-sm text-[var(--text-muted)] py-10">
                {t('sshAssoc.empty')}
              </div>
            ) : (
              <>
                <div className="flex items-center justify-between text-sm text-[var(--text-muted)]">
                  <span>
                    {t('sshAssoc.selectedCount', { checked: checked.size, total: connections.length })}
                  </span>
                  <div className="flex gap-2">
                    <button
                      className="hover:text-[var(--accent)] transition-colors"
                      onClick={() => setCheckedFor(visibleIds, true)}
                    >
                      {t('sshAssoc.selectAll')}
                    </button>
                    <span className="opacity-40">|</span>
                    <button
                      className="hover:text-[var(--accent)] transition-colors"
                      onClick={() => setCheckedFor(visibleIds, false)}
                    >
                      {t('sshAssoc.selectNone')}
                    </button>
                  </div>
                </div>

                {visibleBuckets.map((bucket) => {
                  const key = bucket.group ?? '';
                  const isCollapsed = activeGroup === null && collapsed.has(key);
                  return (
                    <div key={key || '__ungrouped__'} className="space-y-1.5">
                      {activeGroup === null && (bucket.group || hasNamedGroup) && (
                        <button
                          className="w-full flex items-center gap-1.5 text-sm text-[var(--text-muted)] uppercase tracking-[0.1em] hover:text-[var(--text-primary)] transition-colors"
                          onClick={() => toggleCollapsed(key)}
                        >
                          <span className="text-xs w-3 flex-shrink-0">{isCollapsed ? '▸' : '▾'}</span>
                          <span className="truncate">{bucket.group ?? t('sshAssoc.ungrouped')}</span>
                          <span className="normal-case tracking-normal flex-shrink-0">
                            ({bucket.items.length})
                          </span>
                        </button>
                      )}
                      {!isCollapsed &&
                        bucket.items.map((conn) => (
                          <label
                            key={conn.id}
                            className="flex items-center gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] cursor-pointer hover:border-[var(--border-default)] transition-colors"
                          >
                            <input
                              type="checkbox"
                              className="accent-[var(--accent)] flex-shrink-0"
                              checked={checked.has(conn.id)}
                              onChange={() => toggle(conn.id)}
                            />
                            <div className="flex-1 min-w-0">
                              <div className="text-base font-medium text-[var(--text-primary)] truncate">
                                {conn.name}
                              </div>
                              <div className="text-sm text-[var(--text-muted)] font-mono truncate">
                                {connectionSummary(conn)}
                              </div>
                            </div>
                          </label>
                        ))}
                    </div>
                  );
                })}
              </>
            )}
          </div>
        </div>

        {/* 底栏 */}
        <div className="px-5 py-3 border-t border-[var(--border-subtle)] flex items-center gap-3 flex-shrink-0">
          <div className="text-xs text-[var(--text-muted)] flex-1">
            {checked.size === 0
              ? t('sshAssoc.footerHintEmpty')
              : t('sshAssoc.footerHintSelected')}
          </div>
          <button
            className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors disabled:opacity-40"
            onClick={onClose}
            disabled={busy}
          >
            {t('sshAssoc.cancel')}
          </button>
          <button
            className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity disabled:opacity-40"
            onClick={handleSave}
            disabled={busy}
          >
            {busy ? t('sshAssoc.saving') : t('sshAssoc.save')}
          </button>
        </div>
    </Modal>
  );
}

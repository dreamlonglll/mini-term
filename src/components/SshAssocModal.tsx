import { useState, useEffect, useMemo, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { showAlert } from '../utils/prompt';
import { buildGroupBuckets, connectionSummary, GroupSidebarRow } from './SshModal';
import type { SshGroupBucket } from './SshModal';
import { Modal, ModalCloseButton } from './Modal';
import { useT, t as tStatic } from '../i18n';
import type { ProjectConfig } from '../types';

interface Props {
  /** 目标项目；null 表示弹窗关闭 */
  project: ProjectConfig | null;
  onClose: () => void;
}

/** 计算弹窗打开时的初始勾选集合。 */
function initialChecked(project: ProjectConfig, allIds: string[]): Set<string> {
  // 未启用 SSH MCP → 默认全选（保存即以全部范围启用）
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
 * 勾选 ≥1 个连接 = 为该项目启用 SSH MCP 并限定范围；全部取消 = 停用。
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
    const needsScopeMigration = nowEnabled && project.sshConnectionIds === undefined;
    const effectiveUnchanged =
      wasEnabled === nowEnabled &&
      (!nowEnabled || sameScope(project.sshConnectionIds, scope, allIds));

    // 完全无变化且已是显式列表 → 直接关闭，不写盘、不弹提示
    if (effectiveUnchanged && !needsScopeMigration) {
      onClose();
      return;
    }
    // 有效范围不变、仅把旧 undefined 迁移成等价显式列表 → 静默落盘，不弹提示
    const silentMigration = effectiveUnchanged && needsScopeMigration;

    setBusy(true);
    try {
      // 仅启用/停用的切换需要改写 .mcp.json 注册；纯范围变更靠持久化 config 即可，
      // sidecar 每次工具调用都重新读 config.json。
      if (nowEnabled && !wasEnabled) {
        await invoke('enable_ssh_mcp', { projectDir: project.path, projectId: project.id });
      } else if (!nowEnabled && wasEnabled) {
        await invoke('disable_ssh_mcp', { projectDir: project.path });
      }

      const cfg = useAppStore.getState().config;
      const newConfig = {
        ...cfg,
        projects: cfg.projects.map((p) =>
          p.id === project.id
            ? { ...p, sshMcpEnabled: nowEnabled, sshConnectionIds: nowEnabled ? scope : undefined }
            : p,
        ),
      };
      useAppStore.getState().setConfig(newConfig);
      await invoke('save_config', { config: newConfig });
      onClose();

      // 静默迁移(旧 undefined → 等价显式列表,有效范围未变)：落盘即可，不弹提示
      if (silentMigration) return;

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

  if (!project) return null;

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
      open
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
          {t('sshAssoc.subtitle', { name: project.name })}
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

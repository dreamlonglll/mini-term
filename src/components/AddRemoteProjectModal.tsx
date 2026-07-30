import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId, persistConfig } from '../store';
import { buildGroupBuckets, connectionSummary, GroupSidebarRow } from './SshModal';
import type { SshGroupBucket } from './SshModal';
import { Modal, ModalCloseButton } from './Modal';
import { findGroupInTree } from '../utils/projectTree';
import { useT } from '../i18n';
import type { ProjectConfig } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** 从分组右键菜单打开时传入:新项目直接落进该分组（分组折叠则展开） */
  targetGroupId?: string;
}

/**
 * 「添加远程项目」弹窗:选已有 SSH 连接 + 手输远程路径（默认 `~`）。
 * 保存前调 `ssh_remote_validate_dir` 验证目录存在（`~` 由后端 SFTP canonicalize 展开），
 * 用返回的展开绝对路径落 config；项目名默认取路径末段，可编辑。
 * 连接选择区与「SSH 连接」/「关联 SSH」同构：左侧分组栏 + 右侧连接列表。
 * createPortal 到 body（fluent2 [data-panel] backdrop-filter containing block 规避，
 * 见 spec/frontend/fluent2-portal-modal.md）。
 */
export function AddRemoteProjectModal({ open, onClose, targetGroupId }: Props) {
  const t = useT();
  const connections = useAppStore((s) => s.config.sshConnections) ?? [];
  const sshGroups = useAppStore((s) => s.config.sshGroups) ?? [];
  const [connectionId, setConnectionId] = useState('');
  const [path, setPath] = useState('~');
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  /** 左栏选中态：null = 全部；'' = 未分组；其他 = 具名分组名（与 SshModal 一致） */
  const [selectedGroup, setSelectedGroup] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

  useEffect(() => {
    if (open) {
      setConnectionId(connections[0]?.id ?? '');
      setPath('~');
      setName('');
      setBusy(false);
      setError('');
      setSelectedGroup(null);
      setCollapsed(new Set());
    }
    // 仅在弹窗打开时重置一次
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const handleSave = useCallback(async () => {
    if (busy) return;
    if (!connectionId) {
      setError(t('remoteProject.errorNoConnection'));
      return;
    }
    setBusy(true);
    setError('');
    try {
      // 后端:~ 展开 + canonicalize + stat 目录；不存在/非目录/断链 → 明确 Err
      const canonical = await invoke<string>('ssh_remote_validate_dir', {
        connectionId,
        path,
      });
      const finalName =
        name.trim() || canonical.split('/').filter(Boolean).pop() || canonical;
      const project: ProjectConfig = {
        id: genId(),
        name: finalName,
        path: canonical,
        sshConnectionId: connectionId,
      };
      useAppStore.getState().addProject(project);
      if (targetGroupId) {
        useAppStore.getState().moveItem(project.id, targetGroupId);
        // 目标分组若折叠则展开,确保新项目可见（与本地「添加项目」一致）
        const grp = findGroupInTree(useAppStore.getState().config.projectTree ?? [], targetGroupId);
        if (grp?.collapsed) useAppStore.getState().toggleGroupCollapse(targetGroupId);
      }
      useAppStore.getState().setActiveProject(project.id);
      await persistConfig();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }, [busy, connectionId, path, name, onClose, targetGroupId, t]);

  if (!open) return null;

  // 分组归类与 SshModal / SshAssocModal 共用同一份逻辑（含显式创建的空分组）
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
      ariaLabel={t('remoteProject.title')}
      panelClassName="w-[720px] h-[70vh] max-h-[680px]"
      // 保存中不给关：正在做远程校验，中途退出会留下半截状态
      closeOnOverlay={!busy}
      closeOnEscape={!busy}
    >
      {/* 顶栏（带副标题，故不用 Modal 自带的 title） */}
      <div className="px-5 py-4 border-b border-[var(--border-subtle)] flex-shrink-0">
        <div className="flex items-center justify-between gap-3">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('remoteProject.title')}</h2>
          <ModalCloseButton onClose={onClose} label={t('remoteProject.cancel')} />
        </div>
        <div className="text-sm text-[var(--text-muted)] mt-1">
          {t('remoteProject.subtitle')}
        </div>
      </div>

        {/* 内容:上半区「左分组栏 + 右连接列表」与「SSH 连接」弹窗同构,下半区为路径/项目名表单 */}
        {connections.length === 0 ? (
          <div className="flex-1 flex items-center justify-center text-center text-sm text-[var(--text-muted)] px-8 leading-relaxed">
            {t('remoteProject.noConnections')}
          </div>
        ) : (
          <>
            <div className="flex-1 flex min-h-0">
              {/* 左栏 */}
              <div className="w-44 flex-shrink-0 border-r border-[var(--border-subtle)] overflow-y-auto py-2 space-y-0.5">
                <GroupSidebarRow
                  label={t('remoteProject.allConnections')}
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
                    label={t('remoteProject.ungrouped')}
                    count={ungroupedItems.length}
                    active={activeGroup === ''}
                    onClick={() => setSelectedGroup('')}
                  />
                )}
              </div>

              {/* 右栏:连接单选列表 */}
              <div className="flex-1 min-w-0 overflow-y-auto px-5 py-4 space-y-3">
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
                          <span className="truncate">{bucket.group ?? t('remoteProject.ungrouped')}</span>
                          <span className="normal-case tracking-normal flex-shrink-0">
                            ({bucket.items.length})
                          </span>
                        </button>
                      )}
                      {!isCollapsed &&
                        bucket.items.map((conn) => (
                          <label
                            key={conn.id}
                            className={`flex items-center gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border cursor-pointer transition-colors ${
                              connectionId === conn.id
                                ? 'border-[var(--accent)]'
                                : 'border-[var(--border-subtle)] hover:border-[var(--border-default)]'
                            }`}
                          >
                            <input
                              type="radio"
                              name="remote-project-connection"
                              className="accent-[var(--accent)] flex-shrink-0"
                              checked={connectionId === conn.id}
                              onChange={() => setConnectionId(conn.id)}
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
              </div>
            </div>

            {/* 路径 / 项目名（固定在连接区下方,不随列表滚动） */}
            <div className="flex-shrink-0 border-t border-[var(--border-subtle)] px-5 py-4 space-y-3">
              <div className="flex items-center gap-3">
                <div className="w-40 flex-shrink-0 text-sm text-[var(--text-muted)]">
                  {t('remoteProject.pathLabel')}
                </div>
                <input
                  className="flex-1 min-w-0 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
                  placeholder={t('remoteProject.pathPlaceholder')}
                  value={path}
                  spellCheck={false}
                  onChange={(e) => setPath(e.target.value)}
                  onKeyDown={(e) => { if (e.key === 'Enter') void handleSave(); }}
                />
              </div>

              <div className="flex items-center gap-3">
                <div className="w-40 flex-shrink-0 text-sm text-[var(--text-muted)]">
                  {t('remoteProject.nameLabel')}
                </div>
                <input
                  className="flex-1 min-w-0 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)]"
                  placeholder={t('remoteProject.namePlaceholder')}
                  value={name}
                  spellCheck={false}
                  onChange={(e) => setName(e.target.value)}
                  onKeyDown={(e) => { if (e.key === 'Enter') void handleSave(); }}
                />
              </div>

              {error && (
                <div className="text-sm text-[var(--color-error)] break-all">{error}</div>
              )}
            </div>
          </>
        )}

        {/* 底栏 */}
        <div className="px-5 py-3 border-t border-[var(--border-subtle)] flex items-center gap-3 flex-shrink-0">
          <div className="text-xs text-[var(--text-muted)] flex-1">
            {t('remoteProject.footerHint')}
          </div>
          <button
            className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors disabled:opacity-40"
            onClick={onClose}
            disabled={busy}
          >
            {t('remoteProject.cancel')}
          </button>
          <button
            className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity disabled:opacity-40"
            onClick={() => void handleSave()}
            disabled={busy || connections.length === 0}
          >
            {busy ? t('remoteProject.validating') : t('remoteProject.save')}
          </button>
        </div>
    </Modal>
  );
}

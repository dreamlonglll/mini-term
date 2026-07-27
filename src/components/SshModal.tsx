import { useState, useEffect, useCallback, type ReactNode, type MouseEvent } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useAppStore, genId } from '../store';
import { Modal } from './Modal';
import { useT } from '../i18n';
import { showContextMenu } from '../utils/contextMenu';
import { showConfirm } from '../utils/prompt';
import type { SshConnection } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

const INPUT_CLASS =
  'w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)]';

function emptyConnection(group?: string): SshConnection {
  return { id: '', name: '', host: '', port: 22, user: '', group };
}

/** user@host:port 摘要（端口为 22 时省略） */
export function connectionSummary(conn: SshConnection): string {
  const port = conn.port && conn.port !== 22 ? `:${conn.port}` : '';
  return `${conn.user}@${conn.host}${port}`;
}

/** 归一化分组名：trim 后空串视为未分组（undefined） */
function normalizeGroup(group?: string): string | undefined {
  const g = group?.trim();
  return g || undefined;
}

export interface SshGroupBucket {
  /** undefined = 未分组桶 */
  group?: string;
  items: SshConnection[];
}

/**
 * 按分组归类连接。具名分组 = 连接中出现的组（按首次出现顺序）∪ 显式创建的
 * `sshGroups`（允许空组）；未分组连接单独成桶。
 *
 * 「SSH 连接」与「关联 SSH」两个弹窗共用，避免两边分组顺序/空组处理走样。
 */
export function buildGroupBuckets(
  connections: SshConnection[],
  sshGroups: string[],
): { namedGroups: { group: string; items: SshConnection[] }[]; ungroupedItems: SshConnection[] } {
  const namedGroups: { group: string; items: SshConnection[] }[] = [];
  const ensureGroup = (name: string) => {
    let bucket = namedGroups.find((x) => x.group === name);
    if (!bucket) {
      bucket = { group: name, items: [] };
      namedGroups.push(bucket);
    }
    return bucket;
  };
  for (const conn of connections) {
    const g = normalizeGroup(conn.group);
    if (g) ensureGroup(g).items.push(conn);
  }
  for (const raw of sshGroups) {
    const g = raw.trim();
    if (g) ensureGroup(g);
  }
  return { namedGroups, ungroupedItems: connections.filter((c) => !normalizeGroup(c.group)) };
}

// ─── Field（带标签的表单行）───

function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-1">
      <label className="text-sm text-[var(--text-muted)]">{label}</label>
      {children}
      {hint && <div className="text-xs text-[var(--text-muted)]">{hint}</div>}
    </div>
  );
}

// ─── GroupCombobox（分组输入：可下拉选择已有分组，也可输入新分组名）───

function GroupCombobox({
  value,
  onChange,
  options,
  placeholder,
}: {
  value: string;
  onChange: (v: string) => void;
  options: string[];
  placeholder?: string;
}) {
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const keyword = value.trim().toLowerCase();
  const filtered = options.filter((g) => g.toLowerCase().includes(keyword));
  return (
    <div className="relative">
      <input
        className={INPUT_CLASS}
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setDropdownOpen(true);
        }}
        onFocus={() => setDropdownOpen(true)}
        onBlur={() => setDropdownOpen(false)}
        placeholder={placeholder}
      />
      {dropdownOpen && filtered.length > 0 && (
        <div className="absolute left-0 right-0 top-full mt-1 z-10 bg-[var(--bg-elevated)] border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] max-h-36 overflow-y-auto">
          {filtered.map((g) => (
            <div
              key={g}
              className="px-2 py-1 text-sm text-[var(--text-secondary)] cursor-pointer hover:bg-[var(--bg-overlay)] hover:text-[var(--text-primary)] transition-colors"
              onMouseDown={(e) => {
                // 防止 input 先失焦关闭下拉导致点击落空
                e.preventDefault();
                onChange(g);
                setDropdownOpen(false);
              }}
            >
              {g}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ─── SshConnectionForm（新增 / 编辑表单）───

function SshConnectionForm({
  initial,
  groupOptions,
  onSave,
  onCancel,
}: {
  initial: SshConnection;
  groupOptions: string[];
  onSave: (conn: SshConnection) => void;
  onCancel: () => void;
}) {
  const t = useT();
  const [name, setName] = useState(initial.name);
  const [host, setHost] = useState(initial.host);
  const [port, setPort] = useState(String(initial.port || 22));
  const [user, setUser] = useState(initial.user);
  const [password, setPassword] = useState(initial.password ?? '');
  const [identityFile, setIdentityFile] = useState(initial.identityFile ?? '');
  const [group, setGroup] = useState(initial.group ?? '');

  const handleBrowse = useCallback(async () => {
    const selected = await openDialog({ title: t('sshModal.selectKeyFile'), multiple: false, directory: false });
    if (typeof selected === 'string' && selected.trim()) setIdentityFile(selected);
  }, [t]);

  const canSave = !!(name.trim() && host.trim() && user.trim());

  const handleSave = () => {
    if (!canSave) return;
    const parsedPort = parseInt(port, 10);
    onSave({
      id: initial.id || genId(),
      name: name.trim(),
      host: host.trim(),
      port: Number.isFinite(parsedPort) && parsedPort > 0 && parsedPort <= 65535 ? parsedPort : 22,
      user: user.trim(),
      password: password ? password : undefined,
      identityFile: identityFile.trim() || undefined,
      group: normalizeGroup(group),
    });
  };

  return (
    <div className="flex flex-col gap-2.5 p-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--accent)] border-dashed">
      <Field label={t('sshModal.nameLabel')}>
        <input
          className={INPUT_CLASS}
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={t('sshModal.namePlaceholder')}
          autoFocus
        />
      </Field>
      <div className="flex gap-2">
        <div className="flex-[2]">
          <Field label={t('sshModal.hostLabel')}>
            <input
              className={INPUT_CLASS}
              value={host}
              onChange={(e) => setHost(e.target.value)}
              placeholder={t('sshModal.hostPlaceholder')}
            />
          </Field>
        </div>
        <div className="flex-1">
          <Field label={t('sshModal.portLabel')}>
            <input
              className={INPUT_CLASS}
              type="number"
              value={port}
              onChange={(e) => setPort(e.target.value)}
            />
          </Field>
        </div>
      </div>
      <Field label={t('sshModal.userLabel')}>
        <input
          className={INPUT_CLASS}
          value={user}
          onChange={(e) => setUser(e.target.value)}
          placeholder={t('sshModal.userPlaceholder')}
        />
      </Field>
      <Field
        label={t('sshModal.passwordLabel')}
        hint={t('sshModal.passwordHint')}
      >
        <input
          className={INPUT_CLASS}
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </Field>
      <Field label={t('sshModal.identityLabel')} hint={t('sshModal.identityHint')}>
        <div className="flex gap-2">
          <input
            className={INPUT_CLASS}
            value={identityFile}
            onChange={(e) => setIdentityFile(e.target.value)}
            placeholder={t('sshModal.identityPlaceholder')}
          />
          <button
            type="button"
            className="px-3 py-1 text-base bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all flex-shrink-0"
            onClick={handleBrowse}
          >
            ...
          </button>
        </div>
      </Field>
      <Field label={t('sshModal.groupLabel')} hint={t('sshModal.groupHint')}>
        <GroupCombobox
          value={group}
          onChange={setGroup}
          options={groupOptions}
          placeholder={t('sshModal.groupPlaceholder')}
        />
      </Field>
      <div className="flex gap-2 justify-end pt-0.5">
        <button
          className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
          onClick={onCancel}
        >
          {t('sshModal.cancel')}
        </button>
        <button
          className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity disabled:opacity-40"
          onClick={handleSave}
          disabled={!canSave}
        >
          {t('sshModal.save')}
        </button>
      </div>
    </div>
  );
}

// ─── SshRow（连接展示行，可拖拽到左侧分组）───
// WebView2 在 dragDropEnabled 下拦截 HTML5 DnD，与 projectDragState 同理改用鼠标事件模拟拖拽

function SshRow({
  conn,
  onEdit,
  onDelete,
  onMouseDown,
}: {
  conn: SshConnection;
  onEdit: () => void;
  onDelete: () => void;
  onMouseDown: (e: MouseEvent) => void;
}) {
  const t = useT();
  return (
    <div
      className="flex items-center gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] group hover:border-[var(--border-default)] transition-colors cursor-grab active:cursor-grabbing"
      onMouseDown={onMouseDown}
    >
      <div className="flex-1 min-w-0">
        <div className="text-base font-medium text-[var(--text-primary)] truncate">{conn.name}</div>
        <div className="text-sm text-[var(--text-muted)] font-mono truncate">
          {connectionSummary(conn)}
          {conn.password ? t('sshModal.passwordSaved') : ''}
        </div>
      </div>
      <div className="hidden group-hover:flex items-center gap-1">
        <button
          className="px-2 py-0.5 text-sm text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
          onClick={onEdit}
        >
          {t('sshModal.edit')}
        </button>
        <button
          className="px-2 py-0.5 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
          onClick={onDelete}
        >
          {t('sshModal.delete')}
        </button>
      </div>
    </div>
  );
}

// ─── GroupSidebarRow（左侧分组列表项，兼作拖拽落点）───

export function GroupSidebarRow({
  label,
  count,
  active,
  dropActive = false,
  onClick,
  onContextMenu,
  onMouseEnter,
  onMouseLeave,
  onMouseUp,
}: {
  label: string;
  count: number;
  active: boolean;
  /** 拖拽落点高亮；无拖拽场景（如「关联 SSH」）不传 */
  dropActive?: boolean;
  onClick: () => void;
  onContextMenu?: (e: MouseEvent) => void;
  onMouseEnter?: () => void;
  onMouseLeave?: () => void;
  onMouseUp?: () => void;
}) {
  return (
    <div
      className={[
        'flex items-center gap-2 mx-2 px-3 py-1.5 rounded-[var(--radius-sm)] cursor-pointer select-none text-base transition-colors',
        active
          ? 'bg-[var(--bg-overlay)] text-[var(--text-primary)]'
          : 'text-[var(--text-secondary)] hover:bg-[var(--bg-elevated)] hover:text-[var(--text-primary)]',
        dropActive ? 'outline outline-1 outline-dashed outline-[var(--accent)]' : '',
      ].join(' ')}
      onClick={onClick}
      onContextMenu={onContextMenu}
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
      onMouseUp={onMouseUp}
    >
      <span className="flex-1 truncate pointer-events-none">{label}</span>
      <span className="text-xs text-[var(--text-muted)] flex-shrink-0 pointer-events-none">{count}</span>
    </div>
  );
}

// ─── SshModal（主弹窗：左侧分组列表 + 右侧连接列表）───

/** 左栏选中态：null = 全部；'' = 未分组；其他 = 具名分组名（组名已 trim，不会是空串） */
type GroupKey = string | null;

export function SshModal({ open, onClose }: Props) {
  const t = useT();
  const setConfig = useAppStore((s) => s.setConfig);
  const connections = useAppStore((s) => s.config.sshConnections) ?? [];
  const sshGroups = useAppStore((s) => s.config.sshGroups) ?? [];
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [selectedGroup, setSelectedGroup] = useState<GroupKey>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [renamingGroup, setRenamingGroup] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [creatingGroup, setCreatingGroup] = useState(false);
  const [newGroupName, setNewGroupName] = useState('');
  const [dragConnId, setDragConnId] = useState<string | null>(null);
  const [dragOverGroup, setDragOverGroup] = useState<GroupKey>(null);

  useEffect(() => {
    if (open) {
      setAdding(false);
      setEditingId(null);
      setSelectedGroup(null);
      setCollapsed(new Set());
      setRenamingGroup(null);
      setCreatingGroup(false);
      setDragConnId(null);
      setDragOverGroup(null);
    }
  }, [open]);

  const persist = useCallback(
    async (patch: { sshConnections?: SshConnection[]; sshGroups?: string[] }) => {
      const newConfig = { ...useAppStore.getState().config, ...patch };
      setConfig(newConfig);
      await invoke('save_config', { config: newConfig });
    },
    [setConfig],
  );

  const handleSave = (conn: SshConnection) => {
    const current = useAppStore.getState().config.sshConnections ?? [];
    const exists = current.some((c) => c.id === conn.id);
    void persist({
      sshConnections: exists ? current.map((c) => (c.id === conn.id ? conn : c)) : [...current, conn],
    });
    setAdding(false);
    setEditingId(null);
  };

  // 删除不可撤销（密码/私钥路径一并丢失），且会静默收窄已关联项目的 agent 可见范围，
  // 故走二次确认；确认框在 overlayStack 里叠在本弹窗之上，Esc 只关它
  const handleDelete = async (conn: SshConnection) => {
    const ok = await showConfirm(
      t('sshModal.deleteConfirmTitle'),
      t('sshModal.deleteConfirmMessage', { name: conn.name, summary: connectionSummary(conn) }),
    );
    if (!ok) return;
    const current = useAppStore.getState().config.sshConnections ?? [];
    void persist({ sshConnections: current.filter((c) => c.id !== conn.id) });
  };

  // ─── 分组操作 ───

  const renameGroup = (oldName: string) => {
    setRenamingGroup(null);
    const next = renameValue.trim();
    if (!next || next === oldName) return;
    const cfg = useAppStore.getState().config;
    // 连接归属改名 + sshGroups 同步替换（重命名为已有组名时自然合并，去重）
    const seen = new Set<string>();
    const groupsNext: string[] = [];
    for (const raw of cfg.sshGroups ?? []) {
      const n = raw.trim() === oldName ? next : raw.trim();
      if (n && !seen.has(n)) {
        seen.add(n);
        groupsNext.push(n);
      }
    }
    void persist({
      sshConnections: (cfg.sshConnections ?? []).map((c) =>
        normalizeGroup(c.group) === oldName ? { ...c, group: next } : c,
      ),
      sshGroups: groupsNext,
    });
    if (selectedGroup === oldName) setSelectedGroup(next);
    setCollapsed((prev) => {
      if (!prev.has(oldName)) return prev;
      const s = new Set(prev);
      s.delete(oldName);
      s.add(next);
      return s;
    });
  };

  const dissolveGroup = (name: string) => {
    const cfg = useAppStore.getState().config;
    void persist({
      sshConnections: (cfg.sshConnections ?? []).map((c) =>
        normalizeGroup(c.group) === name ? { ...c, group: undefined } : c,
      ),
      sshGroups: (cfg.sshGroups ?? []).filter((n) => n.trim() !== name),
    });
    if (selectedGroup === name) setSelectedGroup(null);
  };

  const createGroup = () => {
    setCreatingGroup(false);
    const name = newGroupName.trim();
    if (!name) return;
    const cfg = useAppStore.getState().config;
    const existing = new Set([
      ...(cfg.sshGroups ?? []).map((n) => n.trim()),
      ...(cfg.sshConnections ?? []).map((c) => normalizeGroup(c.group)).filter(Boolean),
    ]);
    if (!existing.has(name)) {
      void persist({ sshGroups: [...(cfg.sshGroups ?? []), name] });
    }
    setSelectedGroup(name);
  };

  const moveToGroup = (connId: string, group?: string) => {
    const current = useAppStore.getState().config.sshConnections ?? [];
    const conn = current.find((c) => c.id === connId);
    if (!conn || normalizeGroup(conn.group) === group) return;
    void persist({ sshConnections: current.map((c) => (c.id === connId ? { ...c, group } : c)) });
  };

  const handleGroupContextMenu = (e: MouseEvent, name: string) => {
    e.preventDefault();
    e.stopPropagation();
    showContextMenu(e.clientX, e.clientY, [
      {
        label: t('sshModal.renameGroup'),
        onClick: () => {
          setRenameValue(name);
          setRenamingGroup(name);
        },
      },
      {
        label: t('sshModal.dissolveGroup'),
        danger: true,
        onClick: () => dissolveGroup(name),
      },
    ]);
  };

  const handleSidebarContextMenu = (e: MouseEvent) => {
    e.preventDefault();
    showContextMenu(e.clientX, e.clientY, [
      {
        label: t('sshModal.addGroup'),
        onClick: () => {
          setNewGroupName('');
          setCreatingGroup(true);
        },
      },
    ]);
  };

  // ─── 拖拽（鼠标事件模拟，WebView2 dragDropEnabled 会拦截 HTML5 DnD）───

  const handleConnMouseDown = (e: MouseEvent, connId: string) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest('button')) return;
    const el = e.currentTarget as HTMLElement;
    const startX = e.clientX;
    const startY = e.clientY;
    let activated = false;
    const onMove = (me: globalThis.MouseEvent) => {
      if (!activated && Math.abs(me.clientX - startX) + Math.abs(me.clientY - startY) > 5) {
        activated = true;
        el.style.opacity = '0.4';
        document.body.classList.add('ssh-conn-dragging');
        setDragConnId(connId);
      }
    };
    const onUp = () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      el.style.opacity = '';
      document.body.classList.remove('ssh-conn-dragging');
      if (activated) {
        // 抑制紧随 mouseup 的 click，防止误触落点分组行的选中切换
        window.addEventListener(
          'click',
          (ce) => {
            ce.stopPropagation();
            ce.preventDefault();
          },
          { capture: true, once: true },
        );
      }
      setDragConnId(null);
      setDragOverGroup(null);
    };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  };

  /** 生成某个分组落点（'' = 未分组）的鼠标事件回调；组行 onMouseUp 先于全局清理触发 */
  const dropHandlers = (groupKey: string) => ({
    onMouseEnter: () => {
      if (dragConnId) setDragOverGroup(groupKey);
    },
    onMouseLeave: () => {
      if (dragOverGroup === groupKey) setDragOverGroup(null);
    },
    onMouseUp: () => {
      if (dragConnId) moveToGroup(dragConnId, groupKey || undefined);
    },
  });

  if (!open) return null;

  const { namedGroups, ungroupedItems } = buildGroupBuckets(connections, sshGroups);
  const groupOptions = namedGroups.map((g) => g.group);
  const hasNamedGroup = namedGroups.length > 0;
  const groups: SshGroupBucket[] = [
    ...namedGroups,
    ...(ungroupedItems.length > 0 ? [{ group: undefined, items: ungroupedItems }] : []),
  ];

  // 选中的分组可能因删除/重命名/解散而消失，回落到「全部」
  const activeGroup: GroupKey =
    selectedGroup === null
      ? null
      : selectedGroup === ''
        ? ungroupedItems.length > 0 || dragConnId
          ? ''
          : null
        : groupOptions.includes(selectedGroup)
          ? selectedGroup
          : null;

  // 右栏要展示的分组桶：全部视图展示所有桶（带可折叠标题），选中某组只展示该桶
  const visibleBuckets =
    activeGroup === null
      ? groups
      : groups.filter((g) => (g.group ?? '') === activeGroup);

  const toggleCollapsed = (key: string) => {
    setCollapsed((prev) => {
      const s = new Set(prev);
      if (s.has(key)) s.delete(key);
      else s.add(key);
      return s;
    });
  };

  const renderConn = (conn: SshConnection) =>
    editingId === conn.id ? (
      <SshConnectionForm
        key={conn.id}
        initial={conn}
        groupOptions={groupOptions}
        onSave={handleSave}
        onCancel={() => setEditingId(null)}
      />
    ) : (
      <SshRow
        key={conn.id}
        conn={conn}
        onEdit={() => {
          setAdding(false);
          setEditingId(conn.id);
        }}
        onDelete={() => void handleDelete(conn)}
        onMouseDown={(e) => handleConnMouseDown(e, conn.id)}
      />
    );

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={t('sshModal.title')}
      panelClassName="w-[720px] h-[70vh] max-h-[680px]"
      // 面板里有连接表单（含密码/私钥路径），误点遮罩关掉会丢未保存内容；Esc 仍可退
      closeOnOverlay={false}
    >
      {/* 内容：左侧分组列表 + 右侧连接列表 */}
        <div className="flex-1 flex min-h-0">
          {/* 左栏 */}
          <div
            className="w-44 flex-shrink-0 border-r border-[var(--border-subtle)] overflow-y-auto py-2 space-y-0.5"
            onContextMenu={handleSidebarContextMenu}
          >
            <GroupSidebarRow
              label={t('sshModal.allConnections')}
              count={connections.length}
              active={activeGroup === null}
              dropActive={false}
              onClick={() => setSelectedGroup(null)}
            />
            {namedGroups.map((g) =>
              renamingGroup === g.group ? (
                <div key={g.group} className="mx-2 px-1 py-0.5">
                  <input
                    className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--accent)] rounded-[var(--radius-sm)] px-2 py-1 text-sm outline-none"
                    value={renameValue}
                    autoFocus
                    onChange={(e) => setRenameValue(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') renameGroup(g.group);
                      else if (e.key === 'Escape') setRenamingGroup(null);
                    }}
                    onBlur={() => renameGroup(g.group)}
                  />
                </div>
              ) : (
                <GroupSidebarRow
                  key={g.group}
                  label={g.group}
                  count={g.items.length}
                  active={activeGroup === g.group}
                  dropActive={dragOverGroup === g.group}
                  onClick={() => setSelectedGroup(g.group)}
                  onContextMenu={(e) => handleGroupContextMenu(e, g.group)}
                  {...dropHandlers(g.group)}
                />
              ),
            )}
            {(ungroupedItems.length > 0 || dragConnId) && (
              <GroupSidebarRow
                label={t('sshModal.ungrouped')}
                count={ungroupedItems.length}
                active={activeGroup === ''}
                dropActive={dragOverGroup === ''}
                onClick={() => setSelectedGroup('')}
                {...dropHandlers('')}
              />
            )}
            {creatingGroup && (
              <div className="mx-2 px-1 py-0.5">
                <input
                  className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--accent)] rounded-[var(--radius-sm)] px-2 py-1 text-sm outline-none"
                  value={newGroupName}
                  autoFocus
                  placeholder={t('sshModal.addGroupPlaceholder')}
                  onChange={(e) => setNewGroupName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') createGroup();
                    else if (e.key === 'Escape') setCreatingGroup(false);
                  }}
                  onBlur={createGroup}
                />
              </div>
            )}
          </div>

          {/* 右栏 */}
          <div className="flex-1 min-w-0 overflow-y-auto px-5 py-4 space-y-3">
            {connections.length === 0 && !adding && (
              <div className="text-center text-sm text-[var(--text-muted)] py-10">
                {t('sshModal.empty')}
              </div>
            )}

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
                      <span className="truncate">{bucket.group ?? t('sshModal.ungrouped')}</span>
                      <span className="normal-case tracking-normal flex-shrink-0">({bucket.items.length})</span>
                    </button>
                  )}
                  {!isCollapsed && bucket.items.map(renderConn)}
                </div>
              );
            })}

            {adding && (
              <SshConnectionForm
                initial={emptyConnection(activeGroup || undefined)}
                groupOptions={groupOptions}
                onSave={handleSave}
                onCancel={() => setAdding(false)}
              />
            )}

            {!adding && (
              <button
                className="w-full py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
                onClick={() => {
                  setEditingId(null);
                  setAdding(true);
                }}
              >
                {t('sshModal.addConnection')}
              </button>
            )}

            <div className="pt-1 text-sm text-[var(--text-muted)]">
              {t('sshModal.footerHint')}
              <br />
              {t('sshModal.groupOpsHint')}
            </div>
          </div>
        </div>
    </Modal>
  );
}

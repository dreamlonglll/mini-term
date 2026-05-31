import { useState, useEffect, useCallback, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useAppStore, genId } from '../store';
import { useT } from '../i18n';
import type { SshConnection } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

const INPUT_CLASS =
  'w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)]';

function emptyConnection(): SshConnection {
  return { id: '', name: '', host: '', port: 22, user: '' };
}

/** user@host:port 摘要（端口为 22 时省略） */
export function connectionSummary(conn: SshConnection): string {
  const port = conn.port && conn.port !== 22 ? `:${conn.port}` : '';
  return `${conn.user}@${conn.host}${port}`;
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

// ─── SshConnectionForm（新增 / 编辑表单）───

function SshConnectionForm({
  initial,
  onSave,
  onCancel,
}: {
  initial: SshConnection;
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
      group: group.trim() || undefined,
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
        <input
          className={INPUT_CLASS}
          value={group}
          onChange={(e) => setGroup(e.target.value)}
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

// ─── SshRow（连接展示行）───

function SshRow({
  conn,
  onEdit,
  onDelete,
}: {
  conn: SshConnection;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const t = useT();
  return (
    <div className="flex items-center gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] group hover:border-[var(--border-default)] transition-colors">
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

// ─── SshModal（主弹窗）───

export function SshModal({ open, onClose }: Props) {
  const t = useT();
  const setConfig = useAppStore((s) => s.setConfig);
  const connections = useAppStore((s) => s.config.sshConnections) ?? [];
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setAdding(false);
      setEditingId(null);
    }
  }, [open]);

  const persist = useCallback(
    async (next: SshConnection[]) => {
      const newConfig = { ...useAppStore.getState().config, sshConnections: next };
      setConfig(newConfig);
      await invoke('save_config', { config: newConfig });
    },
    [setConfig],
  );

  const handleSave = (conn: SshConnection) => {
    const current = useAppStore.getState().config.sshConnections ?? [];
    const exists = current.some((c) => c.id === conn.id);
    void persist(exists ? current.map((c) => (c.id === conn.id ? conn : c)) : [...current, conn]);
    setAdding(false);
    setEditingId(null);
  };

  const handleDelete = (id: string) => {
    const current = useAppStore.getState().config.sshConnections ?? [];
    void persist(current.filter((c) => c.id !== id));
  };

  if (!open) return null;

  // 按 group 归类，保持首次出现顺序
  const groups: { group?: string; items: SshConnection[] }[] = [];
  for (const conn of connections) {
    const g = conn.group?.trim() || undefined;
    let bucket = groups.find((x) => x.group === g);
    if (!bucket) {
      bucket = { group: g, items: [] };
      groups.push(bucket);
    }
    bucket.items.push(conn);
  }
  const hasNamedGroup = groups.some((g) => g.group);

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh]">
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div className="relative w-[560px] max-h-[80vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in">
        {/* 顶栏 */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border-subtle)]">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('sshModal.title')}</h2>
          <button
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
            onClick={onClose}
          >
            ✕
          </button>
        </div>

        {/* 内容 */}
        <div className="flex-1 overflow-y-auto px-5 py-4 space-y-3">
          {connections.length === 0 && !adding && (
            <div className="text-center text-sm text-[var(--text-muted)] py-10">
              {t('sshModal.empty')}
            </div>
          )}

          {groups.map((bucket) => (
            <div key={bucket.group ?? '__ungrouped__'} className="space-y-1.5">
              {(bucket.group || hasNamedGroup) && (
                <div className="text-sm text-[var(--text-muted)] uppercase tracking-[0.1em]">
                  {bucket.group ?? t('sshModal.ungrouped')}
                </div>
              )}
              {bucket.items.map((conn) =>
                editingId === conn.id ? (
                  <SshConnectionForm
                    key={conn.id}
                    initial={conn}
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
                    onDelete={() => handleDelete(conn.id)}
                  />
                ),
              )}
            </div>
          ))}

          {adding && (
            <SshConnectionForm
              initial={emptyConnection()}
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
          </div>
        </div>
      </div>
    </div>
  );
}

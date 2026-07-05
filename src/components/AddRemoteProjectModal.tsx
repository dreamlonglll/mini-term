import { useState, useEffect, useCallback } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId, persistConfig } from '../store';
import { connectionSummary } from './SshModal';
import { useT } from '../i18n';
import type { ProjectConfig, SshConnection } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

/**
 * 「添加远程项目」弹窗:选已有 SSH 连接 + 手输远程路径（默认 `~`）。
 * 保存前调 `ssh_remote_validate_dir` 验证目录存在（`~` 由后端 SFTP canonicalize 展开），
 * 用返回的展开绝对路径落 config；项目名默认取路径末段，可编辑。
 * createPortal 到 body（fluent2 [data-panel] backdrop-filter containing block 规避，
 * 见 spec/frontend/fluent2-portal-modal.md）。
 */
export function AddRemoteProjectModal({ open, onClose }: Props) {
  const t = useT();
  const connections = useAppStore((s) => s.config.sshConnections) ?? [];
  const [connectionId, setConnectionId] = useState('');
  const [path, setPath] = useState('~');
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    if (open) {
      setConnectionId(connections[0]?.id ?? '');
      setPath('~');
      setName('');
      setBusy(false);
      setError('');
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
      useAppStore.getState().setActiveProject(project.id);
      await persistConfig();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }, [busy, connectionId, path, name, onClose, t]);

  if (!open) return null;

  // 按 group 归类，保持首次出现顺序（与 SshModal / SshAssocModal 一致）
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

  return createPortal(
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh]">
      <div
        className="absolute inset-0 bg-black/50 backdrop-blur-sm"
        onClick={() => { if (!busy) onClose(); }}
      />
      <div className="relative w-[480px] max-h-[80vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in">
        {/* 顶栏 */}
        <div className="px-5 py-4 border-b border-[var(--border-subtle)]">
          <div className="flex items-center justify-between">
            <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('remoteProject.title')}</h2>
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
          <div className="text-sm text-[var(--text-muted)] mt-1">
            {t('remoteProject.subtitle')}
          </div>
        </div>

        {/* 内容 */}
        <div className="flex-1 overflow-y-auto px-5 py-4 space-y-4">
          {connections.length === 0 ? (
            <div className="text-center text-sm text-[var(--text-muted)] py-10 px-4 leading-relaxed">
              {t('remoteProject.noConnections')}
            </div>
          ) : (
            <>
              {/* 连接选择 */}
              <div className="space-y-1.5">
                <div className="text-sm text-[var(--text-muted)]">{t('remoteProject.connectionLabel')}</div>
                <div className="space-y-1 max-h-[220px] overflow-y-auto">
                  {groups.map((bucket) => (
                    <div key={bucket.group ?? '__ungrouped__'} className="space-y-1">
                      {(bucket.group || hasNamedGroup) && (
                        <div className="text-xs text-[var(--text-muted)] uppercase tracking-[0.1em] pt-1">
                          {bucket.group ?? t('remoteProject.ungrouped')}
                        </div>
                      )}
                      {bucket.items.map((conn) => (
                        <label
                          key={conn.id}
                          className={`flex items-center gap-3 px-3 py-2 rounded-[var(--radius-md)] bg-[var(--bg-base)] border cursor-pointer transition-colors ${
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
                            <div className="text-base text-[var(--text-primary)] truncate">{conn.name}</div>
                            <div className="text-sm text-[var(--text-muted)] font-mono truncate">
                              {connectionSummary(conn)}
                            </div>
                          </div>
                        </label>
                      ))}
                    </div>
                  ))}
                </div>
              </div>

              {/* 远程路径 */}
              <div className="space-y-1.5">
                <div className="text-sm text-[var(--text-muted)]">{t('remoteProject.pathLabel')}</div>
                <input
                  className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
                  placeholder={t('remoteProject.pathPlaceholder')}
                  value={path}
                  spellCheck={false}
                  onChange={(e) => setPath(e.target.value)}
                  onKeyDown={(e) => { if (e.key === 'Enter') void handleSave(); }}
                />
              </div>

              {/* 项目名 */}
              <div className="space-y-1.5">
                <div className="text-sm text-[var(--text-muted)]">{t('remoteProject.nameLabel')}</div>
                <input
                  className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)]"
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
            </>
          )}
        </div>

        {/* 底栏 */}
        <div className="px-5 py-3 border-t border-[var(--border-subtle)] flex items-center gap-3">
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
      </div>
    </div>,
    document.body,
  );
}

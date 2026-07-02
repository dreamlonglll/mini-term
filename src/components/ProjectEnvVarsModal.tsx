import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { showAlert } from '../utils/prompt';
import { isWslPath } from '../utils/wslPath';
import { useT, t as tStatic } from '../i18n';
import type { ProjectConfig, ProjectEnvVar } from '../types';

interface Props {
  project: ProjectConfig | null;
  onClose: () => void;
}

interface EditableRow extends ProjectEnvVar {
  rid: string;
}

const KEY_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*$/;

type RowErrorKind =
  | 'empty-key'
  | 'protected-prefix'
  | 'reserved-wslenv'
  | 'invalid-key'
  | 'duplicate-key'
  | 'invalid-value';

const ERROR_TEXT_KEY: Record<RowErrorKind, string> = {
  'empty-key': 'envVars.error.emptyKey',
  'protected-prefix': 'envVars.error.protectedPrefix',
  'reserved-wslenv': 'envVars.error.reservedWslenv',
  'invalid-key': 'envVars.error.invalidKey',
  'duplicate-key': 'envVars.error.duplicateKey',
  'invalid-value': 'envVars.error.invalidValue',
};

/** 错误优先级:空 key > 受保护前缀 > 保留 WSLENV > 非法字符 > 重复 > value 非法 */
function computeErrors(rows: EditableRow[]): Map<string, RowErrorKind> {
  const errors = new Map<string, RowErrorKind>();
  const keyCount = new Map<string, number>();
  for (const r of rows) {
    if (r.key) keyCount.set(r.key, (keyCount.get(r.key) ?? 0) + 1);
  }
  for (const r of rows) {
    if (!r.key.trim() && !r.value.trim()) continue;
    if (!r.key.trim()) {
      errors.set(r.rid, 'empty-key');
      continue;
    }
    if (r.key.startsWith('MINITERM_')) {
      errors.set(r.rid, 'protected-prefix');
      continue;
    }
    // `WSLENV` 在 WSL 分支由 Rust 端拼装注入(K1/u:K2/u: + 宿主既有),
    // 允许用户覆盖会破坏拼接结果。大小写敏感比较 —— 与 Microsoft 官方一致。
    if (r.key === 'WSLENV') {
      errors.set(r.rid, 'reserved-wslenv');
      continue;
    }
    if (!KEY_PATTERN.test(r.key)) {
      errors.set(r.rid, 'invalid-key');
      continue;
    }
    if ((keyCount.get(r.key) ?? 0) > 1) {
      errors.set(r.rid, 'duplicate-key');
      continue;
    }
    if (/[\n\r\0]/.test(r.value)) {
      errors.set(r.rid, 'invalid-value');
      continue;
    }
  }
  return errors;
}

export function ProjectEnvVarsModal({ project, onClose }: Props) {
  const t = useT();
  const [rows, setRows] = useState<EditableRow[]>([]);
  const [busy, setBusy] = useState(false);
  const ridCounter = useRef(0);
  const newKeyInputRef = useRef<HTMLInputElement | null>(null);
  const pendingFocusRid = useRef<string | null>(null);

  const newRid = useCallback(() => `r${++ridCounter.current}`, []);

  useEffect(() => {
    if (!project) return;
    const existing = project.envVars ?? [];
    const initial: EditableRow[] = existing.length > 0
      ? existing.map((e) => ({ ...e, rid: newRid() }))
      : [{ key: '', value: '', enabled: true, rid: newRid() }];
    setRows(initial);
    setBusy(false);
  }, [project, newRid]);

  // 焦点跟随 + 自动滚到底
  useEffect(() => {
    if (pendingFocusRid.current && newKeyInputRef.current) {
      newKeyInputRef.current.focus();
      newKeyInputRef.current.scrollIntoView({ block: 'nearest' });
      pendingFocusRid.current = null;
    }
  });

  // Esc 关闭
  useEffect(() => {
    if (!project) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !busy) onClose();
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [project, busy, onClose]);

  const errors = useMemo(() => computeErrors(rows), [rows]);
  const hasErrors = errors.size > 0;

  const handleAddRow = useCallback(() => {
    const rid = newRid();
    pendingFocusRid.current = rid;
    setRows((prev) => [...prev, { key: '', value: '', enabled: true, rid }]);
  }, [newRid]);

  const handleRemoveRow = useCallback((rid: string) => {
    setRows((prev) => prev.filter((r) => r.rid !== rid));
  }, []);

  const updateRow = useCallback((rid: string, patch: Partial<EditableRow>) => {
    setRows((prev) => prev.map((r) => (r.rid === rid ? { ...r, ...patch } : r)));
  }, []);

  const handleSave = useCallback(async () => {
    if (!project || busy || hasErrors) return;
    setBusy(true);
    // 删除空白占位行,保留有 key 的行(含 enabled=false 的)
    const clean: ProjectEnvVar[] = rows
      .filter((r) => r.key.trim())
      .map((r) => ({ key: r.key, value: r.value, enabled: r.enabled }));

    const prevConfig = useAppStore.getState().config;
    const newConfig = {
      ...prevConfig,
      projects: prevConfig.projects.map((p) =>
        p.id === project.id
          ? { ...p, envVars: clean.length > 0 ? clean : undefined }
          : p,
      ),
    };
    // 乐观更新 store(与 SshAssocModal 一致);失败时回滚到磁盘上的旧值,避免
    // store 与 config.json 不一致导致下次启动丢用户改动。
    useAppStore.getState().setConfig(newConfig);
    try {
      await invoke('save_config', { config: newConfig });
      onClose();
    } catch (e) {
      useAppStore.getState().setConfig(prevConfig);
      setBusy(false);
      await showAlert(tStatic('envVars.saveFailed'), e instanceof Error ? e.message : String(e));
    }
  }, [project, busy, hasErrors, rows, onClose]);

  if (!project) return null;
  const isWsl = isWslPath(project.path);

  return createPortal(
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh]">
      {/* 遮罩:不响应点击,防误触关闭 */}
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div className="relative w-[640px] max-h-[80vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in">
        {/* 顶栏 */}
        <div className="px-5 py-4 border-b border-[var(--border-subtle)]">
          <div className="flex items-center justify-between">
            <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('envVars.title')}</h2>
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none disabled:opacity-40"
              onClick={onClose}
              disabled={busy}
            >
              ✕
            </button>
          </div>
          <div className="text-sm text-[var(--text-muted)] mt-1 truncate">
            {t('envVars.subtitle', { name: project.name })}
          </div>
        </div>

        {isWsl && (
          <div className="mx-5 mt-3 px-3 py-2 rounded bg-green-500/10 border border-green-500/30 text-sm text-green-200">
            ✓ {t('envVars.wsl.part1')}<code className="text-green-100">/u</code>{t('envVars.wsl.part2')}<code className="text-green-100">/home/u/...</code>{t('envVars.wsl.part3')}
            <code className="text-green-100">~/.bashrc</code>{t('envVars.wsl.part4')}<code className="text-green-100">export</code>{t('envVars.wsl.part5')}
          </div>
        )}

        {/* 内容 */}
        <div className="flex-1 overflow-y-auto px-5 py-4">
          {/* 表头 */}
          <div className="flex items-center gap-2 mb-2 text-xs text-[var(--text-muted)] uppercase tracking-wide">
            <span className="w-4 text-center">{t('envVars.colEnabled')}</span>
            <span className="flex-[40] min-w-0">Key</span>
            <span className="flex-[55] min-w-0">Value</span>
            <span className="w-6"></span>
          </div>

          <div className="space-y-1.5">
            {rows.map((row) => {
              const err = errors.get(row.rid);
              const errorBorder = err ? 'border-red-500' : 'border-[var(--border-subtle)]';
              const isPending = pendingFocusRid.current === row.rid;
              return (
                <div key={row.rid}>
                  <div className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      className="w-4 h-4 accent-[var(--accent)] flex-shrink-0"
                      checked={row.enabled}
                      onChange={(e) => updateRow(row.rid, { enabled: e.target.checked })}
                      title={row.enabled ? t('envVars.rowEnabled') : t('envVars.rowDisabled')}
                    />
                    <input
                      ref={isPending ? newKeyInputRef : undefined}
                      type="text"
                      placeholder="KEY"
                      className={`flex-[40] min-w-0 px-2 py-1 text-sm bg-[var(--bg-base)] border ${errorBorder} rounded font-mono outline-none focus:border-[var(--accent)]`}
                      value={row.key}
                      onChange={(e) => updateRow(row.rid, { key: e.target.value })}
                      spellCheck={false}
                      autoCapitalize="off"
                      autoCorrect="off"
                    />
                    <input
                      type="text"
                      placeholder="value"
                      className={`flex-[55] min-w-0 px-2 py-1 text-sm bg-[var(--bg-base)] border ${errorBorder} rounded font-mono outline-none focus:border-[var(--accent)]`}
                      value={row.value}
                      onChange={(e) => updateRow(row.rid, { value: e.target.value })}
                      spellCheck={false}
                      autoCapitalize="off"
                      autoCorrect="off"
                    />
                    <button
                      className="w-6 h-6 flex items-center justify-center text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
                      onClick={() => handleRemoveRow(row.rid)}
                      title={t('envVars.removeRow')}
                    >
                      ✕
                    </button>
                  </div>
                  {err && (
                    <div className="ml-6 mt-0.5 text-xs text-red-400">
                      {t(ERROR_TEXT_KEY[err])}
                    </div>
                  )}
                </div>
              );
            })}
          </div>

          <button
            className="mt-3 text-sm text-[var(--accent)] hover:underline"
            onClick={handleAddRow}
          >
            {t('envVars.addRow')}
          </button>
        </div>

        {/* 底栏 */}
        <div className="px-5 py-3 border-t border-[var(--border-subtle)] flex items-center gap-3">
          <div className="text-xs text-[var(--text-muted)] flex-1">
            {t('envVars.footnote')}
          </div>
          <button
            className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors disabled:opacity-40"
            onClick={onClose}
            disabled={busy}
          >
            {t('envVars.cancel')}
          </button>
          <button
            className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed"
            onClick={handleSave}
            disabled={busy || hasErrors}
            title={hasErrors ? t('envVars.hasErrors') : undefined}
          >
            {busy ? t('envVars.saving') : t('envVars.save')}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}

import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useAppStore } from '../store';
import {
  importProjectToCcConnect,
  importProjectsToCcConnect,
  unlinkProjectFromCcConnect,
} from '../utils/ccConnectImport';
import type { CcConnectConfig, CcConnectStatus, ProjectConfig } from '../types';
import { useT } from '../i18n';

interface Props {
  open: boolean;
  onClose: () => void;
}

/** 未填写可执行文件时回退到 PATH 中的 cc-connect。 */
const DEFAULT_EXE = 'cc-connect';

const DEFAULT_CC_CONNECT_CONFIG: CcConnectConfig = {
  exePath: '',
  configPath: '',
  autoStart: false,
  extraArgs: [],
  projectLinks: {},
};

/**
 * 「连接」弹窗 —— cc-connect 进程管理 + Web Dashboard 入口。
 *
 * 由顶部标题栏「连接」按钮打开,整合原"设置 → cc-connect"页:
 * - 进程生命周期(启动 / 停止 / 重启 / 测试连接 / 编辑配置文件)
 * - 嵌入式 Web Dashboard 入口(running 时可点)
 * - 未填写可执行文件 / 配置路径时回退默认值
 *   (PATH 中的 cc-connect + ~/.cc-connect/config.toml),零配置即可使用
 *
 * open=false 时整体不挂载(内容子组件持有所有 hook),关闭即停止 probe。
 */
export function CcConnectModal({ open, onClose }: Props) {
  if (!open) return null;
  return <CcConnectModalContent onClose={onClose} />;
}

function CcConnectModalContent({ onClose }: { onClose: () => void }) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const ccStatus = useAppStore((s) => s.ccConnectStatus);
  const setCcConnectStatus = useAppStore((s) => s.setCcConnectStatus);
  const openCcDashboard = useAppStore((s) => s.openCcDashboard);

  const cc = config.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;
  const [exePath, setExePath] = useState(cc.exePath);
  const [configPath, setConfigPath] = useState(cc.configPath);
  const [extraArgsInput, setExtraArgsInput] = useState((cc.extraArgs ?? []).join(' '));
  const [resultMsg, setResultMsg] = useState<{ kind: 'ok' | 'err'; text: string } | null>(null);
  const [busy, setBusy] = useState<null | 'start' | 'stop' | 'restart' | 'test'>(null);
  // 勾选待批量导入的项目 id(仅未导入项目可勾选);importing 防止导入/移除期间重复点击
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [importing, setImporting] = useState(false);

  useEffect(() => {
    setExePath(cc.exePath);
    setConfigPath(cc.configPath);
    setExtraArgsInput((cc.extraArgs ?? []).join(' '));
  }, [cc.exePath, cc.configPath, cc.extraArgs]);

  const saveCcConfig = useCallback(async (patch: Partial<CcConnectConfig>) => {
    const current = useAppStore.getState().config.ccConnect ?? DEFAULT_CC_CONNECT_CONFIG;
    const newCc = { ...current, ...patch };
    const newConfig = { ...useAppStore.getState().config, ccConnect: newCc };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
  }, [setConfig]);

  // 用 ref 持有最新 configPath,避免 probe 因为 configPath 变化反复 rebuild
  // (否则用户每键入一个字符都会触发一次探活)
  const configPathRef = useRef(configPath);
  useEffect(() => { configPathRef.current = configPath; }, [configPath]);

  const probe = useCallback(async () => {
    try {
      const status = await invoke<CcConnectStatus>('cc_connect_probe', {
        configPath: configPathRef.current || undefined,
      });
      setCcConnectStatus(status);
      return status;
    } catch (e: unknown) {
      const text = e instanceof Error ? e.message : String(e);
      setCcConnectStatus({ running: false, port: 9820, diagnostic: text });
      return null;
    }
  }, [setCcConnectStatus]);

  // 打开弹窗时立即拉一次状态(全局轮询在弹窗打开期间也会刷新)
  useEffect(() => { void probe(); }, [probe]);

  const handleBrowseExe = useCallback(async () => {
    const isWindows = navigator.userAgent.includes('Windows');
    const selected = await openDialog({
      title: t('ccConnectModal.browseExeTitle'),
      multiple: false,
      directory: false,
      filters: isWindows ? [{ name: t('ccConnectModal.browseExeFilterName'), extensions: ['exe'] }] : undefined,
    });
    if (typeof selected === 'string' && selected.trim()) {
      setExePath(selected);
      void saveCcConfig({ exePath: selected });
    }
  }, [saveCcConfig]);

  const handleBrowseConfig = useCallback(async () => {
    const selected = await openDialog({
      title: t('ccConnectModal.browseConfigTitle'),
      multiple: false,
      directory: false,
      filters: [{ name: 'TOML', extensions: ['toml'] }],
    });
    if (typeof selected === 'string' && selected.trim()) {
      setConfigPath(selected);
      void saveCcConfig({ configPath: selected });
    }
  }, [saveCcConfig]);

  const commitExePath = useCallback(() => {
    const trimmed = exePath.trim();
    if (trimmed !== cc.exePath) void saveCcConfig({ exePath: trimmed });
  }, [exePath, cc.exePath, saveCcConfig]);

  const commitConfigPath = useCallback(() => {
    const trimmed = configPath.trim();
    if (trimmed !== cc.configPath) void saveCcConfig({ configPath: trimmed });
  }, [configPath, cc.configPath, saveCcConfig]);

  const commitExtraArgs = useCallback(() => {
    const parsed = extraArgsInput.trim() ? extraArgsInput.trim().split(/\s+/) : [];
    const same = parsed.length === (cc.extraArgs ?? []).length
      && parsed.every((v, i) => v === cc.extraArgs?.[i]);
    if (!same) void saveCcConfig({ extraArgs: parsed });
  }, [extraArgsInput, cc.extraArgs, saveCcConfig]);

  const handleStart = useCallback(async () => {
    setBusy('start');
    setResultMsg(null);
    try {
      // 未填写时回退 PATH 中的 cc-connect,实现"零配置启动"
      const exe = exePath.trim() || DEFAULT_EXE;
      const pid = await invoke<number>('cc_connect_start', {
        exePath: exe,
        configPath: configPath || undefined,
        extraArgs: cc.extraArgs ?? [],
      });
      setResultMsg({ kind: 'ok', text: t('ccConnectModal.startedOk', { pid }) });
      // 给进程 ~600ms 起监听端口,再拉状态
      setTimeout(() => { void probe(); }, 600);
    } catch (e: unknown) {
      setResultMsg({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(null);
    }
  }, [exePath, configPath, cc.extraArgs, probe]);

  const handleStop = useCallback(async () => {
    setBusy('stop');
    setResultMsg(null);
    try {
      await invoke('cc_connect_stop');
      setResultMsg({ kind: 'ok', text: t('ccConnectModal.stoppedOk') });
      setTimeout(() => { void probe(); }, 400);
    } catch (e: unknown) {
      setResultMsg({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(null);
    }
  }, [probe]);

  const handleRestart = useCallback(async () => {
    setBusy('restart');
    setResultMsg(null);
    try {
      // HTTP /restart 优先;失败回退 kill+spawn 时同样回退默认可执行文件
      await invoke('cc_connect_restart', {
        exePath: exePath.trim() || DEFAULT_EXE,
        configPath: configPath || undefined,
        extraArgs: cc.extraArgs ?? [],
      });
      setResultMsg({ kind: 'ok', text: t('ccConnectModal.restartedOk') });
      setTimeout(() => { void probe(); }, 800);
    } catch (e: unknown) {
      setResultMsg({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBusy(null);
    }
  }, [exePath, configPath, cc.extraArgs, probe]);

  const handleTest = useCallback(async () => {
    setBusy('test');
    setResultMsg(null);
    try {
      const status = await probe();
      if (status?.running) {
        setResultMsg({
          kind: 'ok',
          text: status.version
            ? t('ccConnectModal.testOkWithVersion', { port: status.port, version: status.version })
            : t('ccConnectModal.testOk', { port: status.port }),
        });
      } else {
        setResultMsg({ kind: 'err', text: status?.diagnostic ?? t('ccConnectModal.cannotConnect') });
      }
    } finally {
      setBusy(null);
    }
  }, [probe]);

  const handleOpenConfigToml = useCallback(async () => {
    setResultMsg(null);
    try {
      // 未填写时解析默认 ~/.cc-connect/config.toml 再打开,实现"零配置编辑"
      const trimmed = configPath.trim();
      const target = trimmed || (await invoke<string>('cc_connect_config_path'));
      await invoke('open_path_with_default_app', { path: target });
    } catch (e: unknown) {
      setResultMsg({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    }
  }, [configPath]);

  const handleBatchImport = useCallback(async () => {
    const cfg = useAppStore.getState().config;
    const targets = cfg.projects.filter(
      (p) => selectedIds.has(p.id) && cfg.ccConnect?.projectLinks?.[p.id] === undefined,
    );
    if (targets.length === 0) return;
    setImporting(true);
    try {
      const ok = await importProjectsToCcConnect(targets);
      if (ok) setSelectedIds(new Set());
    } finally {
      setImporting(false);
    }
  }, [selectedIds]);

  const handleSingleImport = useCallback(async (project: ProjectConfig) => {
    setImporting(true);
    try {
      await importProjectToCcConnect(project);
    } finally {
      setImporting(false);
    }
  }, []);

  const handleRemove = useCallback(async (project: ProjectConfig) => {
    setImporting(true);
    try {
      await unlinkProjectFromCcConnect(project);
    } finally {
      setImporting(false);
    }
  }, []);

  const running = ccStatus?.running ?? false;

  // 未导入项目(projectLinks 无记录)+ 勾选交集,全选与批量按钮据此计算(避免已导入残留 id 干扰)
  const unimportedProjects = config.projects.filter((p) => config.ccConnect?.projectLinks?.[p.id] === undefined);
  const selectedCount = unimportedProjects.filter((p) => selectedIds.has(p.id)).length;
  const allSelected = unimportedProjects.length > 0 && selectedCount === unimportedProjects.length;
  const someSelected = selectedCount > 0 && !allSelected;

  // 状态点颜色
  const indicator = (() => {
    if (!ccStatus) return { color: 'var(--text-muted)', label: t('ccConnectModal.indicator.unknown'), glyph: '○' };
    if (ccStatus.running) return { color: 'var(--color-success)', label: t('ccConnectModal.indicator.running'), glyph: '●' };
    if (ccStatus.diagnostic) return { color: 'var(--color-error)', label: t('ccConnectModal.indicator.error'), glyph: '⚠' };
    return { color: 'var(--text-muted)', label: t('ccConnectModal.indicator.stopped'), glyph: '○' };
  })();

  const statusDetail = ccStatus?.running
    ? `${t('ccConnectModal.statusPort', { port: ccStatus.port })}${ccStatus.ownPid ? t('ccConnectModal.statusPid', { pid: ccStatus.ownPid }) : ''}${ccStatus.version ? t('ccConnectModal.statusVersion', { version: ccStatus.version }) : ''}`
    : ccStatus?.diagnostic ?? t('ccConnectModal.statusDetailHint');

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[8vh]" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative w-[600px] max-h-[84vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in"
        onClick={(e) => e.stopPropagation()}
      >
        {/* 顶栏 */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border-subtle)] flex-shrink-0">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('ccConnectModal.title')}</h2>
          <button
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
            onClick={onClose}
          >
            ✕
          </button>
        </div>

        {/* 内容 */}
        <div className="flex-1 overflow-y-auto px-5 py-4 space-y-6">
          {/* 状态指示器 + Dashboard 入口 */}
          <div className="px-3 py-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] space-y-3">
            <div>
              <div className="flex items-center gap-2 mb-1">
                <span data-status-dot style={{ color: indicator.color }} className="text-base leading-none">
                  {indicator.glyph}
                </span>
                <span className="text-base text-[var(--text-primary)]">{t('ccConnectModal.statusRunning', { label: indicator.label })}</span>
              </div>
              <div className="text-sm text-[var(--text-muted)] font-mono break-all">{statusDetail}</div>
            </div>
            <button
              className="w-full py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base font-medium hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed"
              onClick={() => openCcDashboard()}
              disabled={!running}
              title={running ? t('ccConnectModal.openDashboardTitle') : t('ccConnectModal.needStartFirst')}
            >
              {t('ccConnectModal.openDashboard')}
            </button>
          </div>

          {/* 导入项目到 cc-connect:勾选 + 一键导入(批量只重启一次);也可逐行单独导入。
              每个导入项目会附带占位 telegram 平台(后端注入),保证 cc-connect 冷启动,用户后续到 Dashboard 换真平台。 */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between gap-2">
              <span className="text-base text-[var(--text-primary)]">{t('ccConnectModal.importHeading')}</span>
              <div className="flex items-center gap-3">
                {unimportedProjects.length > 0 && (
                  <label className="flex items-center gap-1.5 text-xs text-[var(--text-muted)] cursor-pointer select-none">
                    <input
                      type="checkbox"
                      className="accent-[var(--accent)]"
                      checked={allSelected}
                      ref={(el) => { if (el) el.indeterminate = someSelected; }}
                      onChange={(e) => {
                        setSelectedIds(e.target.checked ? new Set(unimportedProjects.map((p) => p.id)) : new Set());
                      }}
                    />
                    {t('ccConnectModal.selectAll')}
                  </label>
                )}
                <button
                  className="px-2.5 py-1 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed"
                  disabled={!running || importing || selectedCount === 0}
                  title={running ? t('ccConnectModal.batchImportTitle') : t('ccConnectModal.needStartFirst')}
                  onClick={() => { void handleBatchImport(); }}
                >
                  {selectedCount > 0 ? t('ccConnectModal.batchImportWithCount', { count: selectedCount }) : t('ccConnectModal.batchImport')}
                </button>
              </div>
            </div>
            {config.projects.length === 0 ? (
              <div className="text-sm text-[var(--text-muted)] px-3 py-2 rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
                {t('ccConnectModal.noProjects')}
              </div>
            ) : (
              <div className="max-h-[180px] overflow-y-auto rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] divide-y divide-[var(--border-subtle)]">
                {config.projects.map((p) => {
                  const linkedName = config.ccConnect?.projectLinks?.[p.id];
                  const imported = linkedName !== undefined;
                  return (
                    <div key={p.id} className="flex items-center gap-2 px-3 py-2">
                      {imported ? (
                        <span className="w-[13px] flex-shrink-0" aria-hidden />
                      ) : (
                        <input
                          type="checkbox"
                          className="accent-[var(--accent)] flex-shrink-0"
                          checked={selectedIds.has(p.id)}
                          onChange={(e) => {
                            setSelectedIds((prev) => {
                              const next = new Set(prev);
                              if (e.target.checked) next.add(p.id); else next.delete(p.id);
                              return next;
                            });
                          }}
                        />
                      )}
                      <div className="min-w-0 flex-1">
                        <div className="text-sm text-[var(--text-primary)] truncate">{p.name}</div>
                        <div className="text-xs text-[var(--text-muted)] font-mono truncate">{p.path}</div>
                      </div>
                      {imported ? (
                        <div className="flex items-center gap-2 flex-shrink-0">
                          <span className="text-xs text-[var(--color-success)]" title={t('ccConnectModal.importedTitle', { name: linkedName })}>{t('ccConnectModal.importedTag')}</span>
                          <button
                            className="px-2 py-1 text-xs rounded-[var(--radius-sm)] border border-[var(--border-default)] text-[var(--text-secondary)] hover:border-[var(--color-error)] hover:text-[var(--color-error)] transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
                            disabled={!running || importing}
                            title={running ? t('ccConnectModal.removeTitle') : t('ccConnectModal.needStartFirst')}
                            onClick={() => { void handleRemove(p); }}
                          >
                            {t('ccConnectModal.remove')}
                          </button>
                        </div>
                      ) : (
                        <button
                          className="px-2.5 py-1 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed flex-shrink-0"
                          disabled={!running || importing}
                          title={running ? t('ccConnectModal.importBtnTitle') : t('ccConnectModal.needStartFirst')}
                          onClick={() => { void handleSingleImport(p); }}
                        >
                          {t('ccConnectModal.importBtn')}
                        </button>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          {/* 可执行文件路径 */}
          <div className="space-y-1.5">
            <span className="text-base text-[var(--text-primary)]">{t('ccConnectModal.exePathLabel')}</span>
            <div className="flex gap-2 items-center">
              <input
                className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder={t('ccConnectModal.exePathPlaceholder')}
                value={exePath}
                spellCheck={false}
                onChange={(e) => setExePath(e.target.value)}
                onBlur={commitExePath}
                onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
              />
              <button
                type="button"
                className="px-3 py-1.5 text-base bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all flex-shrink-0"
                onClick={handleBrowseExe}
              >
                {t('ccConnectModal.browse')}
              </button>
            </div>
          </div>

          {/* config.toml 路径 */}
          <div className="space-y-1.5">
            <span className="text-base text-[var(--text-primary)]">{t('ccConnectModal.configPathLabel')}</span>
            <div className="flex gap-2 items-center">
              <input
                className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder={t('ccConnectModal.configPathPlaceholder')}
                value={configPath}
                spellCheck={false}
                onChange={(e) => setConfigPath(e.target.value)}
                onBlur={commitConfigPath}
                onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
              />
              <button
                type="button"
                className="px-3 py-1.5 text-base bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all flex-shrink-0"
                onClick={handleBrowseConfig}
              >
                {t('ccConnectModal.browse')}
              </button>
            </div>
          </div>

          {/* 额外启动参数 */}
          <div className="space-y-1.5">
            <span className="text-base text-[var(--text-primary)]">{t('ccConnectModal.extraArgsLabel')}</span>
            <input
              className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
              placeholder={t('ccConnectModal.extraArgsPlaceholder')}
              value={extraArgsInput}
              spellCheck={false}
              onChange={(e) => setExtraArgsInput(e.target.value)}
              onBlur={commitExtraArgs}
              onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
            />
          </div>

          {/* 自动启动 */}
          <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
            <div className="pr-4">
              <div className="text-base text-[var(--text-primary)]">{t('ccConnectModal.autoStartTitle')}</div>
              <div className="text-sm text-[var(--text-muted)]">{t('ccConnectModal.autoStartDesc')}</div>
            </div>
            <button
              className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
                cc.autoStart ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
              }`}
              onClick={() => saveCcConfig({ autoStart: !cc.autoStart })}
            >
              <span
                className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
                  cc.autoStart ? 'translate-x-[18px]' : 'translate-x-0.5'
                }`}
              />
            </button>
          </div>

          {/* 操作按钮组 */}
          <div className="grid grid-cols-3 gap-2">
            <button
              className="py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base hover:opacity-90 transition-opacity disabled:opacity-50"
              onClick={handleStart}
              disabled={busy !== null}
            >
              {busy === 'start' ? t('ccConnectModal.starting') : t('ccConnectModal.start')}
            </button>
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
              onClick={handleStop}
              disabled={busy !== null}
            >
              {busy === 'stop' ? t('ccConnectModal.stopping') : t('ccConnectModal.stop')}
            </button>
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
              onClick={handleRestart}
              disabled={busy !== null}
            >
              {busy === 'restart' ? t('ccConnectModal.restarting') : t('ccConnectModal.restart')}
            </button>
          </div>

          <div className="grid grid-cols-2 gap-2">
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
              onClick={handleTest}
              disabled={busy !== null}
            >
              {busy === 'test' ? t('ccConnectModal.testing') : t('ccConnectModal.test')}
            </button>
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
              onClick={handleOpenConfigToml}
            >
              {t('ccConnectModal.editConfig')}
            </button>
          </div>

          {/* 结果消息 */}
          {resultMsg && (
            <div
              className={`px-3 py-2 rounded-[var(--radius-sm)] bg-[var(--bg-base)] border text-sm whitespace-pre-wrap ${
                resultMsg.kind === 'ok'
                  ? 'border-[var(--color-success)]/30 text-[var(--color-success)]'
                  : 'border-[var(--color-error)]/30 text-[var(--color-error)]'
              }`}
            >
              {resultMsg.text}
            </div>
          )}

          <div className="pt-1 text-sm text-[var(--text-muted)]">
            {t('ccConnectModal.footer')}
          </div>
        </div>
      </div>
    </div>
  );
}

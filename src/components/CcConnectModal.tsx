import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useAppStore } from '../store';
import type { CcConnectConfig, CcConnectStatus } from '../types';

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
      title: '选择 cc-connect 可执行文件',
      multiple: false,
      directory: false,
      filters: isWindows ? [{ name: '可执行文件', extensions: ['exe'] }] : undefined,
    });
    if (typeof selected === 'string' && selected.trim()) {
      setExePath(selected);
      void saveCcConfig({ exePath: selected });
    }
  }, [saveCcConfig]);

  const handleBrowseConfig = useCallback(async () => {
    const selected = await openDialog({
      title: '选择 cc-connect config.toml',
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
      setResultMsg({ kind: 'ok', text: `已启动 cc-connect (pid=${pid})` });
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
      setResultMsg({ kind: 'ok', text: 'cc-connect 已停止' });
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
      setResultMsg({ kind: 'ok', text: '已重启 cc-connect(active sessions 已重连)' });
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
          text: `连接成功:端口 ${status.port}${status.version ? ` · 版本 ${status.version}` : ''}`,
        });
      } else {
        setResultMsg({ kind: 'err', text: status?.diagnostic ?? '无法连接到 cc-connect' });
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

  const running = ccStatus?.running ?? false;

  // 状态点颜色
  const indicator = (() => {
    if (!ccStatus) return { color: 'var(--text-muted)', label: '未知', glyph: '○' };
    if (ccStatus.running) return { color: 'var(--color-success)', label: '运行中', glyph: '●' };
    if (ccStatus.diagnostic) return { color: 'var(--color-error)', label: '错误', glyph: '⚠' };
    return { color: 'var(--text-muted)', label: '未启动', glyph: '○' };
  })();

  const statusDetail = ccStatus?.running
    ? `端口 ${ccStatus.port}${ccStatus.ownPid ? ` · pid ${ccStatus.ownPid}` : ''}${ccStatus.version ? ` · 版本 ${ccStatus.version}` : ''}`
    : ccStatus?.diagnostic ?? '点击下方"测试连接"探测 cc-connect 状态';

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[8vh]" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative w-[600px] max-h-[84vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in"
        onClick={(e) => e.stopPropagation()}
      >
        {/* 顶栏 */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border-subtle)] flex-shrink-0">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">连接 · cc-connect</h2>
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
                <span className="text-base text-[var(--text-primary)]">cc-connect {indicator.label}</span>
              </div>
              <div className="text-sm text-[var(--text-muted)] font-mono break-all">{statusDetail}</div>
            </div>
            <button
              className="w-full py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base font-medium hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed"
              onClick={() => openCcDashboard()}
              disabled={!running}
              title={running ? '打开 cc-connect Web Dashboard' : '需要先启动 cc-connect'}
            >
              打开 Dashboard
            </button>
          </div>

          {/* 可执行文件路径 */}
          <div className="space-y-1.5">
            <span className="text-base text-[var(--text-primary)]">可执行文件路径</span>
            <div className="flex gap-2 items-center">
              <input
                className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder="留空使用 PATH 中的 cc-connect"
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
                浏览
              </button>
            </div>
          </div>

          {/* config.toml 路径 */}
          <div className="space-y-1.5">
            <span className="text-base text-[var(--text-primary)]">config.toml 路径</span>
            <div className="flex gap-2 items-center">
              <input
                className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder="留空使用 ~/.cc-connect/config.toml"
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
                浏览
              </button>
            </div>
          </div>

          {/* 额外启动参数 */}
          <div className="space-y-1.5">
            <span className="text-base text-[var(--text-primary)]">额外启动参数</span>
            <input
              className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
              placeholder="空格分隔,例如:--verbose"
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
              <div className="text-base text-[var(--text-primary)]">mini-term 启动时自动启动</div>
              <div className="text-sm text-[var(--text-muted)]">仅当探测到 cc-connect 未运行时才会 spawn,避免冲突</div>
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
              {busy === 'start' ? '启动中...' : '启动'}
            </button>
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
              onClick={handleStop}
              disabled={busy !== null}
            >
              {busy === 'stop' ? '停止中...' : '停止'}
            </button>
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
              onClick={handleRestart}
              disabled={busy !== null}
            >
              {busy === 'restart' ? '重启中...' : '重启'}
            </button>
          </div>

          <div className="grid grid-cols-2 gap-2">
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
              onClick={handleTest}
              disabled={busy !== null}
            >
              {busy === 'test' ? '测试中...' : '测试连接'}
            </button>
            <button
              className="py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
              onClick={handleOpenConfigToml}
            >
              编辑配置文件
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
            留空可执行文件 / 配置路径即用默认值(PATH 中的 cc-connect + ~/.cc-connect/config.toml)· 重启会断开所有 IM active sessions(chat 历史保留)· mini-term 关闭时不会联动停止 cc-connect
          </div>
        </div>
      </div>
    </div>
  );
}

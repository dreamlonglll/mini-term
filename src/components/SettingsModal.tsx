import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { openUrl } from '@tauri-apps/plugin-opener';
import { useAppStore } from '../store';
import type { ShellConfig } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

type SettingsPage = 'terminal' | 'system' | 'about';

// ─── ShellRow（终端设置子组件）───

function ShellRow({
  shell,
  isDefault,
  onSetDefault,
  onDelete,
  onUpdate,
}: {
  shell: ShellConfig;
  isDefault: boolean;
  onSetDefault: () => void;
  onDelete: () => void;
  onUpdate: (s: ShellConfig) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(shell.name);
  const [command, setCommand] = useState(shell.command);
  const [args, setArgs] = useState(shell.args?.join(' ') ?? '');

  useEffect(() => {
    setName(shell.name);
    setCommand(shell.command);
    setArgs(shell.args?.join(' ') ?? '');
  }, [shell]);

  const handleSave = () => {
    onUpdate({
      name: name.trim() || shell.name,
      command: command.trim() || shell.command,
      args: args.trim() ? args.trim().split(/\s+/) : undefined,
    });
    setEditing(false);
  };

  if (editing) {
    return (
      <div className="flex flex-col gap-2 p-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-default)]">
        <div className="flex gap-2">
          <input
            className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)]"
            placeholder="名称"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <input
            className="flex-[2] bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            placeholder="命令路径"
            value={command}
            onChange={(e) => setCommand(e.target.value)}
          />
        </div>
        <div className="flex gap-2 items-center">
          <input
            className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            placeholder="启动参数（空格分隔，可选）"
            value={args}
            onChange={(e) => setArgs(e.target.value)}
          />
          <button
            className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity"
            onClick={handleSave}
          >
            保存
          </button>
          <button
            className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
            onClick={() => setEditing(false)}
          >
            取消
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex items-center gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] group hover:border-[var(--border-default)] transition-colors">
      <div
        className={`w-3 h-3 rounded-full border-2 cursor-pointer transition-colors flex-shrink-0 ${
          isDefault
            ? 'border-[var(--accent)] bg-[var(--accent)]'
            : 'border-[var(--border-strong)] hover:border-[var(--accent)]'
        }`}
        onClick={onSetDefault}
        title="设为默认"
      />
      <div className="flex-1 min-w-0">
        <div className="text-base font-medium text-[var(--text-primary)]">{shell.name}</div>
        <div className="text-sm text-[var(--text-muted)] font-mono truncate">
          {shell.command}{shell.args ? ` ${shell.args.join(' ')}` : ''}
        </div>
      </div>
      <div className="hidden group-hover:flex items-center gap-1">
        <button
          className="px-2 py-0.5 text-sm text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
          onClick={() => setEditing(true)}
        >
          编辑
        </button>
        <button
          className="px-2 py-0.5 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
          onClick={onDelete}
        >
          删除
        </button>
      </div>
    </div>
  );
}

// ─── TerminalSettings（终端设置页）───

function TerminalSettings() {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const [shells, setShells] = useState<ShellConfig[]>([]);
  const [defaultShell, setDefaultShell] = useState('');
  const [adding, setAdding] = useState(false);
  const [newName, setNewName] = useState('');
  const [newCommand, setNewCommand] = useState('');
  const [newArgs, setNewArgs] = useState('');

  useEffect(() => {
    setShells([...config.availableShells]);
    setDefaultShell(config.defaultShell);
    setAdding(false);
  }, [config]);

  const save = useCallback(async (updatedShells: ShellConfig[], updatedDefault: string) => {
    const newConfig = {
      ...useAppStore.getState().config,
      availableShells: updatedShells,
      defaultShell: updatedDefault,
    };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleAdd = () => {
    if (!newName.trim() || !newCommand.trim()) return;
    const shell: ShellConfig = {
      name: newName.trim(),
      command: newCommand.trim(),
      args: newArgs.trim() ? newArgs.trim().split(/\s+/) : undefined,
    };
    const updated = [...shells, shell];
    setShells(updated);
    setAdding(false);
    setNewName('');
    setNewCommand('');
    setNewArgs('');
    const def = defaultShell || shell.name;
    setDefaultShell(def);
    save(updated, def);
  };

  const handleDelete = (idx: number) => {
    const updated = shells.filter((_, i) => i !== idx);
    setShells(updated);
    const def = updated.find((s) => s.name === defaultShell)
      ? defaultShell
      : updated[0]?.name ?? '';
    setDefaultShell(def);
    save(updated, def);
  };

  const handleUpdate = (idx: number, shell: ShellConfig) => {
    const wasDefault = shells[idx].name === defaultShell;
    const updated = shells.map((s, i) => (i === idx ? shell : s));
    setShells(updated);
    const def = wasDefault ? shell.name : defaultShell;
    setDefaultShell(def);
    save(updated, def);
  };

  const handleSetDefault = (name: string) => {
    setDefaultShell(name);
    save(shells, name);
  };

  return (
    <div className="space-y-2">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        可用终端（●= 默认）
      </div>
      {shells.map((shell, idx) => (
        <ShellRow
          key={`${shell.name}-${idx}`}
          shell={shell}
          isDefault={shell.name === defaultShell}
          onSetDefault={() => handleSetDefault(shell.name)}
          onDelete={() => handleDelete(idx)}
          onUpdate={(s) => handleUpdate(idx, s)}
        />
      ))}

      {adding ? (
        <div className="flex flex-col gap-2 p-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--accent)] border-dashed">
          <div className="flex gap-2">
            <input
              className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)]"
              placeholder="名称（如 pwsh）"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              autoFocus
            />
            <input
              className="flex-[2] bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
              placeholder="命令路径（如 pwsh 或 C:\...\bash.exe）"
              value={newCommand}
              onChange={(e) => setNewCommand(e.target.value)}
            />
          </div>
          <div className="flex gap-2 items-center">
            <input
              className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
              placeholder="启动参数（空格分隔，可选）"
              value={newArgs}
              onChange={(e) => setNewArgs(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && handleAdd()}
            />
            <button
              className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity"
              onClick={handleAdd}
            >
              添加
            </button>
            <button
              className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
              onClick={() => setAdding(false)}
            >
              取消
            </button>
          </div>
        </div>
      ) : (
        <button
          className="w-full py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
          onClick={() => setAdding(true)}
        >
          + 添加终端
        </button>
      )}

      <div className="pt-3 text-sm text-[var(--text-muted)]">
        点击圆点设为默认终端 · 新建终端标签页时可选择类型
      </div>
    </div>
  );
}

// ─── FontSizeSlider ───

function FontSizeSlider({
  label,
  value,
  min,
  max,
  onChange,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  onChange: (v: number) => void;
}) {
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <span className="text-base text-[var(--text-primary)]">{label}</span>
        <span className="text-base font-mono text-[var(--accent)]">{value}px</span>
      </div>
      <div className="flex items-center gap-3">
        <span className="text-sm text-[var(--text-muted)]">{min}</span>
        <input
          type="range"
          min={min}
          max={max}
          step={1}
          value={value}
          onChange={(e) => onChange(Number(e.target.value))}
          className="flex-1 accent-[var(--accent)] h-1.5 cursor-pointer"
        />
        <span className="text-sm text-[var(--text-muted)]">{max}</span>
      </div>
    </div>
  );
}

// ─── SystemSettings（系统设置页）───

function SystemSettings() {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const handleUiFontSizeChange = useCallback((size: number) => {
    const newConfig = { ...useAppStore.getState().config, uiFontSize: size };
    setConfig(newConfig);
    document.documentElement.style.fontSize = `${size}px`;
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleTerminalFontSizeChange = useCallback((size: number) => {
    const newConfig = { ...useAppStore.getState().config, terminalFontSize: size };
    setConfig(newConfig);
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  return (
    <div className="space-y-6">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        字体大小
      </div>

      <FontSizeSlider
        label="界面字体大小"
        value={config.uiFontSize ?? 13}
        min={10}
        max={20}
        onChange={handleUiFontSizeChange}
      />

      <FontSizeSlider
        label="终端字体大小"
        value={config.terminalFontSize ?? 14}
        min={10}
        max={24}
        onChange={handleTerminalFontSizeChange}
      />

      <div className="pt-3 text-sm text-[var(--text-muted)]">
        界面字体影响侧栏、标签页等 UI 元素 · 终端字体影响终端内文字显示
      </div>
    </div>
  );
}

// ─── AboutSettings（关于页）───

const GITHUB_REPO = 'dreamlonglll/mini-term';

interface ReleaseInfo {
  version: string;
  url: string;
  publishedAt: string;
}

function compareVersions(a: string, b: string): number {
  const pa = a.replace(/^v/, '').split('.').map(Number);
  const pb = b.replace(/^v/, '').split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const diff = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

function AboutSettings() {
  const [currentVersion, setCurrentVersion] = useState('');
  const [latest, setLatest] = useState<ReleaseInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    getVersion().then(setCurrentVersion);
  }, []);

  const checkUpdate = useCallback(async () => {
    setChecking(true);
    setError('');
    setLatest(null);
    try {
      const resp = await fetch(`https://api.github.com/repos/${GITHUB_REPO}/releases/latest`);
      if (!resp.ok) throw new Error(resp.status === 404 ? '暂无发布版本' : `请求失败 (${resp.status})`);
      const data = await resp.json();
      setLatest({
        version: data.tag_name,
        url: data.html_url,
        publishedAt: data.published_at,
      });
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : '检查失败，请稍后重试');
    } finally {
      setChecking(false);
    }
  }, []);

  const hasUpdate = latest && compareVersions(latest.version, currentVersion) > 0;

  return (
    <div className="space-y-6">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        版本信息
      </div>

      {/* 当前版本 */}
      <div className="flex items-center gap-3 px-4 py-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <span className="text-base text-[var(--text-secondary)]">当前版本</span>
        <span className="font-mono text-base text-[var(--accent)]">v{currentVersion}</span>
      </div>

      {/* 检查更新按钮 */}
      <button
        className="w-full py-2.5 border border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-secondary)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50 disabled:cursor-not-allowed"
        onClick={checkUpdate}
        disabled={checking}
      >
        {checking ? '正在检查...' : '检查更新'}
      </button>

      {/* 检查结果 */}
      {error && (
        <div className="px-4 py-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--color-error)]/30 text-sm text-[var(--color-error)]">
          {error}
        </div>
      )}

      {latest && (
        <div className={`px-4 py-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border ${hasUpdate ? 'border-[var(--accent)]/50' : 'border-[var(--border-subtle)]'}`}>
          {hasUpdate ? (
            <div className="space-y-3">
              <div className="flex items-center gap-2">
                <span className="text-base text-[var(--text-primary)]">发现新版本</span>
                <span className="font-mono text-base text-[var(--accent)]">{latest.version}</span>
              </div>
              <div className="text-sm text-[var(--text-muted)]">
                发布于 {new Date(latest.publishedAt).toLocaleDateString('zh-CN')}
              </div>
              <button
                className="w-full py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base font-medium hover:opacity-90 transition-opacity"
                onClick={() => openUrl(latest.url)}
              >
                前往 GitHub 下载
              </button>
            </div>
          ) : (
            <div className="text-base text-[var(--text-secondary)]">
              已是最新版本
            </div>
          )}
        </div>
      )}

      <div className="pt-3 text-sm text-[var(--text-muted)]">
        点击检查更新从 GitHub 获取最新版本信息
      </div>
    </div>
  );
}

// ─── SettingsModal（主弹窗）───

const MENU_ITEMS: { key: SettingsPage; label: string }[] = [
  { key: 'terminal', label: '终端设置' },
  { key: 'system', label: '系统设置' },
  { key: 'about', label: '关于' },
];

export function SettingsModal({ open, onClose }: Props) {
  const [activePage, setActivePage] = useState<SettingsPage>('terminal');

  useEffect(() => {
    if (open) setActivePage('terminal');
  }, [open]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative w-[640px] max-h-[80vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-2xl flex flex-col overflow-hidden animate-slide-in"
        onClick={(e) => e.stopPropagation()}
      >
        {/* 顶栏 */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border-subtle)]">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">设置</h2>
          <button
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
            onClick={onClose}
          >
            ✕
          </button>
        </div>

        {/* 左右布局 */}
        <div className="flex flex-1 overflow-hidden">
          {/* 左侧菜单 */}
          <div className="w-[160px] flex-shrink-0 border-r border-[var(--border-subtle)] py-3 px-2 space-y-0.5">
            {MENU_ITEMS.map((item) => (
              <div
                key={item.key}
                className={`flex items-center gap-2 px-3 py-2 rounded-[var(--radius-sm)] cursor-pointer text-base transition-all duration-150 ${
                  activePage === item.key
                    ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                    : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
                }`}
                onClick={() => setActivePage(item.key)}
              >
                {activePage === item.key && (
                  <span className="w-0.5 h-4 rounded-full bg-[var(--accent)] flex-shrink-0" />
                )}
                <span>{item.label}</span>
              </div>
            ))}
          </div>

          {/* 右侧内容 */}
          <div className="flex-1 overflow-y-auto px-5 py-4">
            {activePage === 'terminal' && <TerminalSettings />}
            {activePage === 'system' && <SystemSettings />}
            {activePage === 'about' && <AboutSettings />}
          </div>
        </div>
      </div>
    </div>
  );
}

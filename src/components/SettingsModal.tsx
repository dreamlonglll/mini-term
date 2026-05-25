import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { openUrl } from '@tauri-apps/plugin-opener';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useAppStore } from '../store';
import { playNotificationSound } from '../utils/notificationSound';
import { checkForUpdate, compareVersions, type ReleaseInfo } from '../utils/updateChecker';
import { applyTheme } from '../utils/themeManager';
import { updateAllTerminalThemes, DEFAULT_TERMINAL_FONT_FAMILY } from '../utils/terminalCache';
import { applyUiFontFamily } from '../utils/fontManager';
import { MOD_LABEL } from '../utils/platform';
import type { ShellConfig, EditorConfig } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

type SettingsPage = 'terminal' | 'system' | 'font' | 'ai-notification' | 'shortcuts' | 'about';

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

// ─── EditorRow（编辑器设置子组件）───

function EditorRow({
  editor,
  isDefault,
  onSetDefault,
  onDelete,
  onUpdate,
  onBrowse,
}: {
  editor: EditorConfig;
  isDefault: boolean;
  onSetDefault: () => void;
  onDelete: () => void;
  onUpdate: (e: EditorConfig) => void;
  onBrowse: (onSelect: (path: string) => void) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(editor.name);
  const [command, setCommand] = useState(editor.command);

  useEffect(() => {
    setName(editor.name);
    setCommand(editor.command);
  }, [editor]);

  const handleSave = () => {
    onUpdate({
      name: name.trim() || editor.name,
      command: command.trim() || editor.command,
    });
    setEditing(false);
  };

  if (editing) {
    return (
      <div className="flex flex-col gap-2 p-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-default)]">
        <input
          className="bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)]"
          placeholder="名称"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <div className="flex gap-2 items-center">
          <input
            className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            placeholder="可执行文件路径"
            value={command}
            onChange={(e) => setCommand(e.target.value)}
          />
          <button
            type="button"
            className="px-3 py-1 text-base bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all flex-shrink-0"
            onClick={() => onBrowse((p) => setCommand(p))}
          >
            ...
          </button>
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
        <div className="text-base font-medium text-[var(--text-primary)]">{editor.name}</div>
        <div className="text-sm text-[var(--text-muted)] font-mono truncate">{editor.command}</div>
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

  const longPasteEnabled = config.longPasteToFile ?? true;
  const smartCopyPasteEnabled = config.smartCopyPaste ?? false;
  const savedLineThreshold = config.longPasteLineThreshold ?? 10;
  const savedCharThreshold = config.longPasteCharThreshold ?? 2000;
  const [lineThresholdInput, setLineThresholdInput] = useState(String(savedLineThreshold));
  const [charThresholdInput, setCharThresholdInput] = useState(String(savedCharThreshold));

  useEffect(() => {
    setShells([...config.availableShells]);
    setDefaultShell(config.defaultShell);
    setAdding(false);
  }, [config]);

  useEffect(() => {
    setLineThresholdInput(String(savedLineThreshold));
    setCharThresholdInput(String(savedCharThreshold));
  }, [savedLineThreshold, savedCharThreshold]);

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

  const saveConfigPatch = useCallback(async (patch: Partial<typeof config>) => {
    const newConfig = { ...useAppStore.getState().config, ...patch };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleLongPasteEnabledChange = (enabled: boolean) => {
    void saveConfigPatch({ longPasteToFile: enabled });
  };

  const commitLineThreshold = () => {
    const n = parseInt(lineThresholdInput, 10);
    const clamped = Number.isFinite(n) && n >= 0 ? Math.min(n, 100000) : savedLineThreshold;
    setLineThresholdInput(String(clamped));
    if (clamped !== savedLineThreshold) {
      void saveConfigPatch({ longPasteLineThreshold: clamped });
    }
  };

  const commitCharThreshold = () => {
    const n = parseInt(charThresholdInput, 10);
    const clamped = Number.isFinite(n) && n >= 0 ? Math.min(n, 10000000) : savedCharThreshold;
    setCharThresholdInput(String(clamped));
    if (clamped !== savedCharThreshold) {
      void saveConfigPatch({ longPasteCharThreshold: clamped });
    }
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

      <div className="pt-6 text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        复制粘贴
      </div>

      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <div className="pr-4">
          <div className="text-base text-[var(--text-primary)]">智能 Ctrl+C / Ctrl+V</div>
          <div className="text-sm text-[var(--text-muted)]">
            开启后选中文本时 Ctrl+C 复制、无选区时中断程序，Ctrl+V 直接粘贴；Ctrl+Shift+C/V 始终可用
          </div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
            smartCopyPasteEnabled ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => saveConfigPatch({ smartCopyPaste: !smartCopyPasteEnabled })}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              smartCopyPasteEnabled ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div className="pt-6 text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        长文本粘贴
      </div>

      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <div className="pr-4">
          <div className="text-base text-[var(--text-primary)]">粘贴时超长文本转临时文件</div>
          <div className="text-sm text-[var(--text-muted)]">
            超过下方阈值时，将剪贴板内容保存到临时 .txt 并粘贴带引号的文件路径，避免 AI 工具被超长粘贴卡住
          </div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
            longPasteEnabled ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => handleLongPasteEnabledChange(!longPasteEnabled)}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              longPasteEnabled ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div
        className={`flex items-center justify-between gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] transition-opacity ${
          longPasteEnabled ? '' : 'opacity-50 pointer-events-none'
        }`}
      >
        <div className="flex-1 min-w-0">
          <div className="text-base text-[var(--text-primary)]">行数阈值</div>
          <div className="text-sm text-[var(--text-muted)]">粘贴内容行数 ≥ 此值即转存（0 表示不按行数判断）</div>
        </div>
        <input
          type="number"
          min={0}
          className="w-24 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono text-right"
          value={lineThresholdInput}
          onChange={(e) => setLineThresholdInput(e.target.value)}
          onBlur={commitLineThreshold}
          onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
        />
      </div>

      <div
        className={`flex items-center justify-between gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] transition-opacity ${
          longPasteEnabled ? '' : 'opacity-50 pointer-events-none'
        }`}
      >
        <div className="flex-1 min-w-0">
          <div className="text-base text-[var(--text-primary)]">字符数阈值</div>
          <div className="text-sm text-[var(--text-muted)]">粘贴内容长度 ≥ 此值即转存（0 表示不按字符判断）</div>
        </div>
        <input
          type="number"
          min={0}
          className="w-24 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono text-right"
          value={charThresholdInput}
          onChange={(e) => setCharThresholdInput(e.target.value)}
          onBlur={commitCharThreshold}
          onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
        />
      </div>

      <div className="pt-1 text-sm text-[var(--text-muted)]">
        关闭后超长文本将直接粘贴 · 任一阈值命中即触发转存 · 临时文件保存在系统 temp 目录，24 小时后自动清理
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

  const [editors, setEditors] = useState<EditorConfig[]>([]);
  const [defaultEditorName, setDefaultEditorName] = useState('');
  const [addingEditor, setAddingEditor] = useState(false);
  const [newEditorName, setNewEditorName] = useState('');
  const [newEditorCommand, setNewEditorCommand] = useState('');

  useEffect(() => {
    setEditors([...config.editors]);
    setDefaultEditorName(config.defaultEditor ?? '');
    setAddingEditor(false);
  }, [config]);

  const saveEditors = useCallback(async (updatedEditors: EditorConfig[], updatedDefault: string) => {
    const newConfig = {
      ...useAppStore.getState().config,
      editors: updatedEditors,
      defaultEditor: updatedDefault || undefined,
    };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleAddEditor = useCallback(() => {
    if (!newEditorName.trim() || !newEditorCommand.trim()) return;
    const trimmedName = newEditorName.trim();
    if (editors.some((e) => e.name === trimmedName)) {
      alert(`已存在名为「${trimmedName}」的编辑器，请使用其他名称`);
      return;
    }
    const editor: EditorConfig = {
      name: trimmedName,
      command: newEditorCommand.trim(),
    };
    const updated = [...editors, editor];
    setEditors(updated);
    setAddingEditor(false);
    setNewEditorName('');
    setNewEditorCommand('');
    const def = defaultEditorName || editor.name;
    setDefaultEditorName(def);
    saveEditors(updated, def);
  }, [editors, defaultEditorName, newEditorName, newEditorCommand, saveEditors]);

  const handleDeleteEditor = useCallback((idx: number) => {
    const updated = editors.filter((_, i) => i !== idx);
    setEditors(updated);
    const def = updated.find((e) => e.name === defaultEditorName)
      ? defaultEditorName
      : updated[0]?.name ?? '';
    setDefaultEditorName(def);
    saveEditors(updated, def);
  }, [editors, defaultEditorName, saveEditors]);

  const handleUpdateEditor = useCallback((idx: number, editor: EditorConfig) => {
    const oldName = editors[idx].name;
    if (editor.name !== oldName && editors.some((e, i) => i !== idx && e.name === editor.name)) {
      alert(`已存在名为「${editor.name}」的编辑器，请使用其他名称`);
      return;
    }
    const wasDefault = oldName === defaultEditorName;
    const updated = editors.map((e, i) => (i === idx ? editor : e));
    setEditors(updated);
    const def = wasDefault ? editor.name : defaultEditorName;
    setDefaultEditorName(def);
    saveEditors(updated, def);
  }, [editors, defaultEditorName, saveEditors]);

  const handleSetDefaultEditor = useCallback((name: string) => {
    setDefaultEditorName(name);
    saveEditors(editors, name);
  }, [editors, saveEditors]);

  const handleBrowseEditorPath = useCallback(async (onSelect: (path: string) => void) => {
    const isWindows = navigator.userAgent.includes('Windows');
    const selected = await openDialog({
      title: '选择编辑器可执行文件',
      multiple: false,
      directory: false,
      filters: isWindows
        ? [{ name: '可执行文件', extensions: ['exe'] }]
        : undefined,
    });
    if (typeof selected === 'string' && selected.trim()) {
      onSelect(selected);
    }
  }, []);

  const handleThemeChange = useCallback((theme: 'auto' | 'light' | 'dark') => {
    const newConfig = { ...useAppStore.getState().config, theme };
    setConfig(newConfig);
    applyTheme(theme);
    updateAllTerminalThemes(newConfig.terminalFollowTheme ?? true);
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleTerminalFollowThemeChange = useCallback((follow: boolean) => {
    const newConfig = { ...useAppStore.getState().config, terminalFollowTheme: follow };
    setConfig(newConfig);
    updateAllTerminalThemes(follow);
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleSkinChange = useCallback((skin: 'none' | 'blueprint') => {
    const currentConfig = useAppStore.getState().config;
    const newConfig = { ...currentConfig, skin };
    setConfig(newConfig);
    updateAllTerminalThemes(newConfig.terminalFollowTheme);
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  return (
    <div className="space-y-6">
      {/* 主题模式 */}
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        主题
      </div>

      <div className="flex gap-2 mb-4">
        {([
          { value: 'dark' as const, label: '深色' },
          { value: 'light' as const, label: '浅色' },
          { value: 'auto' as const, label: '跟随系统' },
        ]).map((opt) => (
          <button
            key={opt.value}
            className={`flex-1 py-2 rounded-[var(--radius-sm)] text-base transition-all ${
              config.theme === opt.value
                ? 'bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)]'
                : 'bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)]'
            }`}
            onClick={() => handleThemeChange(opt.value)}
          >
            {opt.label}
          </button>
        ))}
      </div>

      {/* 皮肤 */}
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2 mt-4">
        皮肤
      </div>

      <div className="flex gap-2 mb-4">
        {([
          { value: 'none' as const, label: '无' },
          { value: 'blueprint' as const, label: '蓝图' },
        ]).map((opt) => (
          <button
            key={opt.value}
            className={`flex-1 py-2 rounded-[var(--radius-sm)] text-base transition-all ${
              config.skin === opt.value
                ? 'bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)]'
                : 'bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)]'
            }`}
            onClick={() => handleSkinChange(opt.value)}
          >
            {opt.label}
          </button>
        ))}
      </div>

      {/* 终端跟随主题 */}
      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] mb-6">
        <div>
          <div className="text-base text-[var(--text-primary)]">终端跟随主题</div>
          <div className="text-sm text-[var(--text-muted)]">关闭时终端始终使用深色方案</div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors ${
            config.terminalFollowTheme ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => handleTerminalFollowThemeChange(!config.terminalFollowTheme)}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              config.terminalFollowTheme ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      {/* 外部编辑器 */}
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        外部编辑器（● = 默认）
      </div>

      <div className="space-y-2 mb-6">
        {editors.map((editor, idx) => (
          <EditorRow
            key={`${editor.name}-${idx}`}
            editor={editor}
            isDefault={editor.name === defaultEditorName}
            onSetDefault={() => handleSetDefaultEditor(editor.name)}
            onDelete={() => handleDeleteEditor(idx)}
            onUpdate={(e) => handleUpdateEditor(idx, e)}
            onBrowse={handleBrowseEditorPath}
          />
        ))}

        {addingEditor ? (
          <div className="flex flex-col gap-2 p-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--accent)] border-dashed">
            <div className="flex gap-2">
              <input
                className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)]"
                placeholder="名称（如 VS Code）"
                value={newEditorName}
                onChange={(e) => setNewEditorName(e.target.value)}
                autoFocus
              />
            </div>
            <div className="flex gap-2 items-center">
              <input
                className="flex-[2] bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder="可执行文件路径"
                value={newEditorCommand}
                onChange={(e) => setNewEditorCommand(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && handleAddEditor()}
              />
              <button
                type="button"
                className="px-3 py-1 text-base bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all flex-shrink-0"
                onClick={() => handleBrowseEditorPath((p) => setNewEditorCommand(p))}
              >
                ...
              </button>
              <button
                className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity"
                onClick={handleAddEditor}
              >
                添加
              </button>
              <button
                className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
                onClick={() => setAddingEditor(false)}
              >
                取消
              </button>
            </div>
          </div>
        ) : (
          <button
            className="w-full py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
            onClick={() => setAddingEditor(true)}
          >
            + 添加编辑器
          </button>
        )}

        <div className="pt-1 text-sm text-[var(--text-muted)]">
          点击圆点设为默认编辑器 · 文件树顶部按钮将使用默认编辑器打开
        </div>
      </div>
    </div>
  );
}

// ─── FontSettings（字体设置页）───

function FontSettings() {
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

  const handleUiFontFamilyChange = useCallback((value: string) => {
    const trimmed = value.trim();
    const newConfig = {
      ...useAppStore.getState().config,
      uiFontFamily: trimmed || undefined,
    };
    setConfig(newConfig);
    applyUiFontFamily(trimmed || undefined);
    invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleTerminalFontFamilyChange = useCallback((value: string) => {
    const trimmed = value.trim();
    const newConfig = {
      ...useAppStore.getState().config,
      terminalFontFamily: trimmed || undefined,
    };
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

      <div className="pt-4 text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        字体
      </div>

      <FontFamilyInput
        label="界面字体"
        value={config.uiFontFamily ?? ''}
        placeholder="'DM Sans', system-ui, sans-serif"
        onChange={handleUiFontFamilyChange}
      />

      <FontFamilyInput
        label="终端字体"
        value={config.terminalFontFamily ?? ''}
        placeholder={DEFAULT_TERMINAL_FONT_FAMILY}
        onChange={handleTerminalFontFamilyChange}
      />

      <div className="pt-3 text-sm text-[var(--text-muted)]">
        留空使用默认 · 支持 CSS font-family 语法（如 <span className="font-mono">'JetBrainsMono Nerd Font', monospace</span>）
      </div>
    </div>
  );
}

// ─── FontFamilyInput ───

function FontFamilyInput({
  label,
  value,
  placeholder,
  onChange,
}: {
  label: string;
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => {
    setDraft(value);
  }, [value]);
  const commit = () => {
    if (draft !== value) onChange(draft);
  };
  return (
    <div className="space-y-1.5">
      <span className="text-base text-[var(--text-primary)]">{label}</span>
      <input
        type="text"
        value={draft}
        placeholder={placeholder}
        spellCheck={false}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            (e.target as HTMLInputElement).blur();
          }
        }}
        className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1.5 text-base outline-none focus:border-[var(--accent)] font-mono"
      />
    </div>
  );
}

// ─── AiNotificationSettings（AI 完成通知页）───

function AiHookSettings() {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const hookEnabled = config.hookEnabled ?? false;

  const [hookStatus, setHookStatus] = useState<{ port: number; running: boolean } | null>(null);
  const [registering, setRegistering] = useState(false);
  const [unregistering, setUnregistering] = useState(false);
  const [toggling, setToggling] = useState(false);
  const [resultMsg, setResultMsg] = useState('');
  const [snippetData, setSnippetData] = useState<{
    claude: { file: string; content: string };
    codex: { files: { file: string; content: string; note?: string }[] };
  } | null>(null);
  const [showSnippet, setShowSnippet] = useState(false);
  const [snippetTab, setSnippetTab] = useState<'claude' | 'codex'>('claude');

  const refreshHookStatus = useCallback(() => {
    invoke<{ port: number; running: boolean }>('get_hook_status').then(setHookStatus);
  }, []);

  useEffect(() => {
    refreshHookStatus();
  }, [refreshHookStatus]);

  const handleToggleHook = useCallback(async (enabled: boolean) => {
    setToggling(true);
    try {
      await invoke('toggle_hook_server', { enabled });
      const newConfig = { ...useAppStore.getState().config, hookEnabled: enabled };
      setConfig(newConfig);
      await invoke('save_config', { config: newConfig });
      refreshHookStatus();
    } catch (e: unknown) {
      setResultMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setToggling(false);
    }
  }, [setConfig, refreshHookStatus]);

  const handleRegister = useCallback(async () => {
    setRegistering(true);
    setResultMsg('');
    try {
      const msg = await invoke<string>('register_ai_hooks');
      setResultMsg(msg);
    } catch (e: unknown) {
      setResultMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setRegistering(false);
    }
  }, []);

  const handleUnregister = useCallback(async () => {
    setUnregistering(true);
    setResultMsg('');
    try {
      const msg = await invoke<string>('unregister_ai_hooks');
      setResultMsg(msg);
    } catch (e: unknown) {
      setResultMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setUnregistering(false);
    }
  }, []);

  const handleShowSnippet = useCallback(async () => {
    if (showSnippet) {
      setShowSnippet(false);
      return;
    }
    try {
      const data = await invoke<typeof snippetData>('get_hook_config_snippet');
      setSnippetData(data);
      setShowSnippet(true);
    } catch (e: unknown) {
      setSnippetData(null);
      setShowSnippet(true);
    }
  }, [showSnippet]);

  return (
    <div className="space-y-2">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        AI Hook 事件
      </div>

      {/* Hook 服务器开关 */}
      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <div className="pr-4">
          <div className="text-base text-[var(--text-primary)]">启用 Hook 服务器</div>
          <div className="text-sm text-[var(--text-muted)]">
            开启后监听本地端口接收 AI 事件，Windows 上首次开启可能触发防火墙授权
          </div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
            hookEnabled ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => !toggling && handleToggleHook(!hookEnabled)}
          disabled={toggling}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              hookEnabled ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      {/* 错误消息始终可见（不受开关置灰影响） */}
      {resultMsg && (
        <div className="px-3 py-2 rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-default)] text-sm text-[var(--text-secondary)] whitespace-pre-wrap">
          {resultMsg}
        </div>
      )}

      {/* 开关关闭时整块置灰 */}
      <div className={`space-y-2 transition-opacity ${hookEnabled ? '' : 'opacity-50 pointer-events-none'}`}>
        <div className="px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
          <div className="flex items-center gap-2 mb-1">
            <div className={`w-2 h-2 rounded-full ${hookStatus?.running ? 'bg-[var(--color-success)]' : 'bg-[var(--border-strong)]'}`} />
            <span className="text-base text-[var(--text-primary)]">
              Hook 服务器 {hookStatus?.running ? `运行中 (端口 ${hookStatus.port})` : '未启动'}
            </span>
          </div>
          <div className="text-sm text-[var(--text-muted)]">
            通过 Claude Code / Codex 的 hook 系统获取精确的 AI 状态，替代进程轮询
          </div>
        </div>

        <div className="flex gap-2">
          <button
            className="flex-1 py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base hover:opacity-90 transition-opacity disabled:opacity-50"
            onClick={handleRegister}
            disabled={registering}
          >
            {registering ? '注册中...' : '一键注册 Hook'}
          </button>
          <button
            className="flex-1 py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
            onClick={handleUnregister}
            disabled={unregistering}
          >
            {unregistering ? '卸载中...' : '卸载 Hook'}
          </button>
        </div>

        <button
          className="w-full py-2 text-base text-[var(--text-muted)] hover:text-[var(--accent)] transition-colors"
          onClick={handleShowSnippet}
        >
          {showSnippet ? '收起配置片段' : '查看配置片段（手动粘贴）'}
        </button>

        {showSnippet && snippetData && (
          <div className="rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-default)] overflow-hidden">
            <div className="flex border-b border-[var(--border-subtle)]">
              {(['claude', 'codex'] as const).map((tab) => (
                <button
                  key={tab}
                  className={`flex-1 py-1.5 text-sm transition-colors ${
                    snippetTab === tab
                      ? 'text-[var(--accent)] border-b-2 border-[var(--accent)]'
                      : 'text-[var(--text-muted)] hover:text-[var(--text-secondary)]'
                  }`}
                  onClick={() => setSnippetTab(tab)}
                >
                  {tab === 'claude' ? 'Claude Code' : 'Codex'}
                </button>
              ))}
            </div>
            <div className="px-3 py-2 text-xs font-mono text-[var(--text-muted)] whitespace-pre-wrap max-h-64 overflow-y-auto select-all">
              {snippetTab === 'claude' ? (
                <>
                  <div className="text-[var(--text-secondary)] mb-1">{snippetData.claude.file}</div>
                  {snippetData.claude.content}
                </>
              ) : (
                snippetData.codex.files.map((f, i) => (
                  <div key={f.file} className={i > 0 ? 'mt-3 pt-3 border-t border-[var(--border-subtle)]' : ''}>
                    <div className="text-[var(--text-secondary)] mb-1">
                      {f.file}{f.note ? ` (${f.note})` : ''}
                    </div>
                    {f.content}
                  </div>
                ))
              )}
            </div>
          </div>
        )}

        <div className="pt-1 text-sm text-[var(--text-muted)]">
          注册后 Claude Code / Codex 会在事件发生时通知 Mini-Term · 未注册时自动使用进程轮询检测
        </div>
      </div>
    </div>
  );
}

// ─── AiNotificationSettings（AI 完成通知页）───

function AiNotificationSettings() {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const saveConfigPatch = useCallback(async (patch: Partial<typeof config>) => {
    const newConfig = { ...useAppStore.getState().config, ...patch };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
  }, [setConfig]);

  const handleSoundPathChange = useCallback(async () => {
    const selected = await openDialog({
      title: '选择提示音文件',
      multiple: false,
      directory: false,
      filters: [{ name: '音频文件', extensions: ['mp3', 'wav', 'ogg', 'flac', 'aac', 'm4a'] }],
    });
    if (typeof selected === 'string' && selected.trim()) {
      void saveConfigPatch({ aiCompletionSoundPath: selected });
    }
  }, [saveConfigPatch]);

  return (
    <div className="space-y-2">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        通知方式
      </div>

      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <div>
          <div className="text-base text-[var(--text-primary)]">弹框提醒</div>
          <div className="text-sm text-[var(--text-muted)]">AI 任务结束时在右下角弹出提醒卡片</div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
            config.aiCompletionPopup ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => saveConfigPatch({ aiCompletionPopup: !config.aiCompletionPopup })}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              config.aiCompletionPopup ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <div>
          <div className="text-base text-[var(--text-primary)]">任务栏闪烁</div>
          <div className="text-sm text-[var(--text-muted)]">窗口失焦时请求用户注意（Windows 闪烁任务栏，macOS 跳动 Dock）</div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
            config.aiCompletionTaskbarFlash ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => saveConfigPatch({ aiCompletionTaskbarFlash: !config.aiCompletionTaskbarFlash })}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              config.aiCompletionTaskbarFlash ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <div>
          <div className="text-base text-[var(--text-primary)]">提示音</div>
          <div className="text-sm text-[var(--text-muted)]">AI 任务结束时播放提示音</div>
        </div>
        <button
          className={`relative w-9 h-5 rounded-full transition-colors flex-shrink-0 ${
            config.aiCompletionSound ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
          }`}
          onClick={() => saveConfigPatch({ aiCompletionSound: !config.aiCompletionSound })}
        >
          <span
            className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
              config.aiCompletionSound ? 'translate-x-[18px]' : 'translate-x-0.5'
            }`}
          />
        </button>
      </div>

      <div
        className={`transition-opacity ${
          config.aiCompletionSound ? '' : 'opacity-50 pointer-events-none'
        }`}
      >
        <div className="flex items-center gap-2 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
          <div className="flex-1 min-w-0">
            <div className="text-base text-[var(--text-primary)]">自定义提示音</div>
            <div className="text-sm text-[var(--text-muted)] font-mono truncate">
              {config.aiCompletionSoundPath || '默认提示音'}
            </div>
          </div>
          <div className="flex items-center gap-1 flex-shrink-0">
            <button
              className="px-2.5 py-1 text-sm bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
              onClick={() => playNotificationSound(config.aiCompletionSoundPath)}
              title="试听"
            >
              试听
            </button>
            <button
              className="px-2.5 py-1 text-sm bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
              onClick={handleSoundPathChange}
            >
              选择文件
            </button>
            {config.aiCompletionSoundPath && (
              <button
                className="px-2.5 py-1 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
                onClick={() => saveConfigPatch({ aiCompletionSoundPath: undefined })}
              >
                清除
              </button>
            )}
          </div>
        </div>
      </div>

      <div className="pt-3 text-sm text-[var(--text-muted)]">
        以上通知在 AI 任务（Claude / Codex）完成时触发
      </div>

      <div className="pt-6">
        <AiHookSettings />
      </div>
    </div>
  );
}

// ─── AboutSettings（关于页）───

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
      const release = await checkForUpdate(currentVersion);
      if (release) {
        setLatest(release);
      } else {
        // 没有新版本，仍显示当前为最新
        setLatest({ version: currentVersion, url: '', publishedAt: '' });
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : '检查失败，请稍后重试');
    } finally {
      setChecking(false);
    }
  }, [currentVersion]);

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

// ─── ShortcutsSettings（快捷键页）───

function buildShortcutGroups(
  smartCopyPaste: boolean,
): { title: string; items: { keys: string; desc: string }[] }[] {
  const terminalItems = smartCopyPaste
    ? [
        { keys: `${MOD_LABEL} + C`, desc: '复制选中文本（无选区时中断程序）' },
        { keys: `${MOD_LABEL} + V`, desc: '粘贴到终端' },
        { keys: `${MOD_LABEL} + Shift + C`, desc: '复制终端选中文本' },
        { keys: `${MOD_LABEL} + Shift + V`, desc: '粘贴到终端' },
      ]
    : [
        { keys: `${MOD_LABEL} + Shift + C`, desc: '复制终端选中文本' },
        { keys: `${MOD_LABEL} + Shift + V`, desc: '粘贴到终端' },
      ];
  return [
    {
      title: '全局',
      items: [{ keys: `${MOD_LABEL} + Shift + F`, desc: '打开/关闭全局搜索' }],
    },
    {
      title: '终端操作',
      items: terminalItems,
    },
    {
      title: 'AI 任务标记',
      items: [
        { keys: `${MOD_LABEL} + Shift + ↑`, desc: '跳转到上一个 AI 任务提交' },
        { keys: `${MOD_LABEL} + Shift + ↓`, desc: '跳转到下一个 AI 任务提交' },
      ],
    },
  ];
}

function ShortcutsSettings() {
  const smartCopyPaste = useAppStore((s) => s.config.smartCopyPaste ?? false);
  const groups = buildShortcutGroups(smartCopyPaste);
  return (
    <div className="space-y-6">
      {groups.map((group) => (
        <div key={group.title}>
          <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
            {group.title}
          </div>
          <div className="space-y-1">
            {group.items.map((item) => (
              <div
                key={item.keys}
                className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]"
              >
                <span className="text-base text-[var(--text-primary)]">{item.desc}</span>
                <kbd className="px-2 py-0.5 rounded-[var(--radius-sm)] bg-[var(--bg-elevated)] border border-[var(--border-default)] text-sm font-mono text-[var(--text-secondary)]">
                  {item.keys}
                </kbd>
              </div>
            ))}
          </div>
        </div>
      ))}
      <div className="pt-3 text-sm text-[var(--text-muted)]">
        终端内快捷键仅在终端获得焦点时生效
      </div>
    </div>
  );
}

// ─── SettingsModal（主弹窗）───

const MENU_ITEMS: { key: SettingsPage; label: string }[] = [
  { key: 'terminal', label: '终端设置' },
  { key: 'system', label: '系统设置' },
  { key: 'font', label: '字体' },
  { key: 'ai-notification', label: 'AI 完成通知' },
  { key: 'shortcuts', label: '快捷键' },
  { key: 'about', label: '关于' },
];

export function SettingsModal({ open, onClose }: Props) {
  const [activePage, setActivePage] = useState<SettingsPage>('terminal');

  useEffect(() => {
    if (open) setActivePage('terminal');
  }, [open]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh]" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative w-[640px] max-h-[80vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in"
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
            {activePage === 'font' && <FontSettings />}
            {activePage === 'ai-notification' && <AiNotificationSettings />}
            {activePage === 'shortcuts' && <ShortcutsSettings />}
            {activePage === 'about' && <AboutSettings />}
          </div>
        </div>
      </div>
    </div>
  );
}

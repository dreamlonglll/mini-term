import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import type { ShellConfig } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

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

export function TerminalConfigModal({ open, onClose }: Props) {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const [shells, setShells] = useState<ShellConfig[]>([]);
  const [defaultShell, setDefaultShell] = useState('');
  const [adding, setAdding] = useState(false);
  const [newName, setNewName] = useState('');
  const [newCommand, setNewCommand] = useState('');
  const [newArgs, setNewArgs] = useState('');

  useEffect(() => {
    if (open) {
      setShells([...config.availableShells]);
      setDefaultShell(config.defaultShell);
      setAdding(false);
    }
  }, [open, config]);

  const save = async (updatedShells: ShellConfig[], updatedDefault: string) => {
    const newConfig = {
      ...config,
      availableShells: updatedShells,
      defaultShell: updatedDefault,
    };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
  };

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

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative w-[480px] max-h-[80vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-2xl flex flex-col overflow-hidden animate-slide-in"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border-subtle)]">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">终端配置</h2>
          <button
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
            onClick={onClose}
          >
            ✕
          </button>
        </div>

        <div className="flex-1 overflow-y-auto px-5 py-4 space-y-2">
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
        </div>

        <div className="px-5 py-3 border-t border-[var(--border-subtle)] text-sm text-[var(--text-muted)]">
          点击圆点设为默认终端 · 新建终端标签页时可选择类型
        </div>
      </div>
    </div>
  );
}

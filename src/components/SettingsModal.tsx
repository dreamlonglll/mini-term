import { useState, useEffect, useCallback, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { openUrl } from '@tauri-apps/plugin-opener';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useAppStore, saveConfigToDisk } from '../store';
import { playNotificationSound } from '../utils/notificationSound';
import { checkForUpdate, compareVersions, type ReleaseInfo } from '../utils/updateChecker';
import { applyTheme } from '../utils/themeManager';
import {
  updateAllTerminalThemes,
  updateAllTerminalScrollback,
  resolveScrollback,
  MAX_SCROLLBACK,
  DEFAULT_TERMINAL_FONT_FAMILY,
} from '../utils/terminalCache';
import { applyUiFontFamily } from '../utils/fontManager';
import { clearCustomTheme, invalidateThemeAssets, listThemePacks, loadAndApplyCustomTheme, resolveThemeAssetUrl, type ThemePackMeta } from '../utils/themePackManager';
import { MOD_LABEL } from '../utils/platform';
import { comboLabel, hotkeyGroups } from '../utils/hotkeys';
import { DEFAULT_REMOTE_PASTE_DIR } from '../utils/pastePath';
import { useT } from '../i18n';
import { LanguageToggle } from './LanguageToggle';
import { Modal } from './Modal';
import type { AppConfig, ShellConfig, EditorConfig } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** 打开时定位到指定页(便于深链入口直达某个设置分区)。 */
  initialPage?: SettingsPage;
}

/**
 * 设置分页 id。**旧 id 一律保留**（`terminal` / `system` / `font` / `ai-notification`），
 * 拆页时只把内容挪走、不改 key —— 外部深链（`initialPage`）不会因为重排失效。
 */
export type SettingsPage =
  | 'terminal'
  | 'clipboard'
  | 'appearance'
  | 'font'
  | 'ai-notification'
  | 'ai-hook'
  | 'system'
  | 'editor'
  | 'shortcuts'
  | 'about';

// ─── 通用原语 ───
//
// 设置项的三种形态（开关 / 数字 / 单选段）此前在各页各写一遍，同一个 toggle 的
// 15 行 JSX 复制了十来份，改一处样式要翻遍全文件。这里收成三个组件，
// 各页只描述「这项设置是什么」。

/** 写一份 config 补丁并落盘。所有设置页共用，避免每页各抄一份 setConfig+save。 */
function useConfigPatch() {
  const setConfig = useAppStore((s) => s.setConfig);
  return useCallback(
    (patch: Partial<AppConfig>) => {
      const newConfig = { ...useAppStore.getState().config, ...patch };
      setConfig(newConfig);
      void saveConfigToDisk(newConfig);
      return newConfig;
    },
    [setConfig],
  );
}

/** 分节：标题 + 若干设置行。页面根节点用 space-y-6 隔开各节。 */
function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-2">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em]">{title}</div>
      {children}
    </section>
  );
}

/** 分节末尾的补充说明。 */
function Hint({ children }: { children: ReactNode }) {
  return <div className="text-sm text-[var(--text-muted)]">{children}</div>;
}

/** 一行设置：左侧标题 + 说明，右侧控件。 */
function SettingRow({
  title,
  desc,
  disabled,
  children,
}: {
  title: ReactNode;
  desc?: ReactNode;
  /** 置灰并屏蔽交互（依赖某个开关的从属项） */
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <div
      className={`flex items-center justify-between gap-3 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] transition-opacity ${
        disabled ? 'opacity-50 pointer-events-none' : ''
      }`}
    >
      <div className="flex-1 min-w-0">
        <div className="text-base text-[var(--text-primary)]">{title}</div>
        {desc !== undefined && <div className="text-sm text-[var(--text-muted)]">{desc}</div>}
      </div>
      <div className="flex-shrink-0">{children}</div>
    </div>
  );
}

function Toggle({
  checked,
  onChange,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      className={`relative w-9 h-5 rounded-full transition-colors ${
        checked ? 'bg-[var(--accent)]' : 'bg-[var(--border-strong)]'
      }`}
      onClick={() => !disabled && onChange(!checked)}
    >
      <span
        className={`absolute top-0.5 left-0 w-4 h-4 rounded-full bg-white transition-transform ${
          checked ? 'translate-x-[18px]' : 'translate-x-0.5'
        }`}
      />
    </button>
  );
}

function ToggleRow({
  title,
  desc,
  checked,
  onChange,
  disabled,
  busy,
}: {
  title: ReactNode;
  desc?: ReactNode;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  /** 正在提交（开关本身仍可见可点，只是这一刻不响应） */
  busy?: boolean;
}) {
  return (
    <SettingRow title={title} desc={desc} disabled={disabled}>
      <Toggle checked={checked} onChange={onChange} disabled={busy} />
    </SettingRow>
  );
}

/**
 * 数字设置行。输入期间只改草稿，失焦/回车才归一并提交 ——
 * 边打字边 clamp 会让「1000」在敲到「1」时就被吃掉。
 * `clamp` 返回 null 表示这次输入无效，回落到已保存值。
 */
function NumberRow({
  title,
  desc,
  value,
  min,
  max,
  step,
  float,
  clamp,
  onCommit,
  disabled,
}: {
  title: ReactNode;
  desc?: ReactNode;
  value: number;
  min?: number;
  max?: number;
  step?: number;
  float?: boolean;
  clamp?: (n: number) => number | null;
  onCommit: (v: number) => void;
  disabled?: boolean;
}) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => {
    setDraft(String(value));
  }, [value]);

  const commit = () => {
    const n = float ? parseFloat(draft) : parseInt(draft, 10);
    const normalize =
      clamp ??
      ((v: number) =>
        Number.isFinite(v) && v >= (min ?? 0) ? Math.min(v, max ?? Number.MAX_SAFE_INTEGER) : null);
    const next = normalize(n) ?? value;
    setDraft(String(next));
    if (next !== value) onCommit(next);
  };

  return (
    <SettingRow title={title} desc={desc} disabled={disabled}>
      <input
        type="number"
        min={min}
        max={max}
        step={step}
        className="w-24 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono text-right"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
      />
    </SettingRow>
  );
}

/** 单选段控件（主题 / 皮肤这类互斥选项）。 */
function ChoiceGroup<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
}) {
  return (
    <div className="flex gap-2">
      {options.map((opt) => (
        <button
          key={opt.value}
          type="button"
          className={`flex-1 py-2 rounded-[var(--radius-sm)] text-base transition-all ${
            value === opt.value
              ? 'bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)]'
              : 'bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)]'
          }`}
          onClick={() => onChange(opt.value)}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}

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
  const t = useT();
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
            placeholder={t("settings.common.namePlaceholder")}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <input
            className="flex-[2] bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            placeholder={t("settings.shellRow.commandPlaceholder")}
            value={command}
            onChange={(e) => setCommand(e.target.value)}
          />
        </div>
        <div className="flex gap-2 items-center">
          <input
            className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            placeholder={t("settings.shellRow.argsPlaceholder")}
            value={args}
            onChange={(e) => setArgs(e.target.value)}
          />
          <button
            className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity"
            onClick={handleSave}
          >
            {t("settings.common.save")}
          </button>
          <button
            className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
            onClick={() => setEditing(false)}
          >
            {t("settings.common.cancel")}
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
        title={t("settings.common.setDefault")}
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
          {t("settings.common.edit")}
        </button>
        <button
          className="px-2 py-0.5 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
          onClick={onDelete}
        >
          {t("settings.common.delete")}
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
  const t = useT();
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
          placeholder={t("settings.common.namePlaceholder")}
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <div className="flex gap-2 items-center">
          <input
            className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            placeholder={t("settings.editorRow.execPathPlaceholder")}
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
            {t("settings.common.save")}
          </button>
          <button
            className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
            onClick={() => setEditing(false)}
          >
            {t("settings.common.cancel")}
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
        title={t("settings.common.setDefault")}
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
          {t("settings.common.edit")}
        </button>
        <button
          className="px-2 py-0.5 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
          onClick={onDelete}
        >
          {t("settings.common.delete")}
        </button>
      </div>
    </div>
  );
}

// ─── TerminalSettings（终端 › Shell）───

function TerminalSettings() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const patchConfig = useConfigPatch();

  const [shells, setShells] = useState<ShellConfig[]>([]);
  const [defaultShell, setDefaultShell] = useState('');
  const [adding, setAdding] = useState(false);
  const [newName, setNewName] = useState('');
  const [newCommand, setNewCommand] = useState('');
  const [newArgs, setNewArgs] = useState('');

  const savedScrollback = resolveScrollback(config.terminalScrollback);

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
    await saveConfigToDisk(newConfig);
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
    <div className="space-y-6">
      <Section title={t("settings.terminal.availableTerminals")}>
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
                placeholder={t("settings.terminal.newNamePlaceholder")}
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                autoFocus
              />
              <input
                className="flex-[2] bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder={t("settings.terminal.newCommandPlaceholder")}
                value={newCommand}
                onChange={(e) => setNewCommand(e.target.value)}
              />
            </div>
            <div className="flex gap-2 items-center">
              <input
                className="flex-1 bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder={t("settings.terminal.newArgsPlaceholder")}
                value={newArgs}
                onChange={(e) => setNewArgs(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && handleAdd()}
              />
              <button
                className="px-3 py-1 text-base bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] hover:opacity-90 transition-opacity"
                onClick={handleAdd}
              >
                {t("settings.common.add")}
              </button>
              <button
                className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
                onClick={() => setAdding(false)}
              >
                {t("settings.common.cancel")}
              </button>
            </div>
          </div>
        ) : (
          <button
            className="w-full py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
            onClick={() => setAdding(true)}
          >
            {t("settings.terminal.addTerminal")}
          </button>
        )}

        <Hint>{t("settings.terminal.defaultHint")}</Hint>
      </Section>

      <Section title={t("settings.terminal.behavior")}>
        <NumberRow
          title={t("settings.terminal.scrollback")}
          desc={t("settings.terminal.scrollbackDesc")}
          value={savedScrollback}
          min={0}
          max={MAX_SCROLLBACK}
          step={1000}
          onCommit={(v) => {
            // 立即对已开终端生效:调小时 xterm 当场裁掉多余历史并释放内存,
            // 内存吃紧的用户不用重启就能看到效果
            updateAllTerminalScrollback(v);
            patchConfig({ terminalScrollback: v });
          }}
        />
      </Section>
    </div>
  );
}

// ─── ClipboardSettings（终端 › 复制粘贴）───

function ClipboardSettings() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const patchConfig = useConfigPatch();

  const smartCopyPasteEnabled = config.smartCopyPaste ?? false;
  const longPasteEnabled = config.longPasteToFile ?? true;
  const savedAutoCopySecs = config.selectionAutoCopySecs ?? 1;
  const savedLineThreshold = config.longPasteLineThreshold ?? 10;
  const savedCharThreshold = config.longPasteCharThreshold ?? 2000;
  const savedRemotePasteDir = config.remotePasteDir ?? DEFAULT_REMOTE_PASTE_DIR;

  const [remotePasteDirInput, setRemotePasteDirInput] = useState(savedRemotePasteDir);
  useEffect(() => {
    setRemotePasteDirInput(savedRemotePasteDir);
  }, [savedRemotePasteDir]);

  const commitRemotePasteDir = () => {
    // 清空 = 回默认值（而不是落一个空串让后端每次去兜底）。
    // `..` 的拒绝在后端 resolve_paste_dir，这里只做归一，避免两处判定漂移。
    const next = remotePasteDirInput.trim() || DEFAULT_REMOTE_PASTE_DIR;
    setRemotePasteDirInput(next);
    if (next !== savedRemotePasteDir) patchConfig({ remotePasteDir: next });
  };

  return (
    <div className="space-y-6">
      <Section title={t("settings.clipboard.copyPaste")}>
        <ToggleRow
          title={t("settings.clipboard.smartCopyPasteTitle")}
          desc={t("settings.clipboard.smartCopyPasteDesc")}
          checked={smartCopyPasteEnabled}
          onChange={(v) => patchConfig({ smartCopyPaste: v })}
        />
        <NumberRow
          title={t("settings.clipboard.autoCopyDwellTitle")}
          desc={t("settings.clipboard.autoCopyDwellDesc")}
          value={savedAutoCopySecs}
          min={0.2}
          step={0.5}
          float
          // 0 = 关闭该功能(静默覆盖剪贴板的行为必须可退出);非零值钳在 0.2~60s
          clamp={(n) =>
            !Number.isFinite(n) || n < 0 ? null : n === 0 ? 0 : Math.min(Math.max(n, 0.2), 60)
          }
          onCommit={(v) => patchConfig({ selectionAutoCopySecs: v })}
        />
      </Section>

      <Section title={t("settings.clipboard.longPaste")}>
        <ToggleRow
          title={t("settings.clipboard.longPasteTitle")}
          desc={t("settings.clipboard.longPasteDesc")}
          checked={longPasteEnabled}
          onChange={(v) => patchConfig({ longPasteToFile: v })}
        />
        <NumberRow
          title={t("settings.clipboard.lineThreshold")}
          desc={t("settings.clipboard.lineThresholdDesc")}
          value={savedLineThreshold}
          min={0}
          max={100000}
          disabled={!longPasteEnabled}
          onCommit={(v) => patchConfig({ longPasteLineThreshold: v })}
        />
        <NumberRow
          title={t("settings.clipboard.charThreshold")}
          desc={t("settings.clipboard.charThresholdDesc")}
          value={savedCharThreshold}
          min={0}
          max={10000000}
          disabled={!longPasteEnabled}
          onCommit={(v) => patchConfig({ longPasteCharThreshold: v })}
        />
        <Hint>{t("settings.clipboard.longPasteFooter")}</Hint>
      </Section>

      <Section title={t("settings.clipboard.remotePaste")}>
        <div className="px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
          <div className="text-base text-[var(--text-primary)]">
            {t("settings.clipboard.remotePasteDir")}
          </div>
          <div className="text-sm text-[var(--text-muted)] mb-2">
            {t("settings.clipboard.remotePasteDirDesc")}
          </div>
          <input
            type="text"
            spellCheck={false}
            placeholder={DEFAULT_REMOTE_PASTE_DIR}
            className="w-full bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
            value={remotePasteDirInput}
            onChange={(e) => setRemotePasteDirInput(e.target.value)}
            onBlur={commitRemotePasteDir}
            onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
          />
        </div>
        <Hint>{t("settings.clipboard.remotePasteFooter")}</Hint>
      </Section>
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

// ─── AppearanceSettings（外观 › 主题与语言）───

function AppearanceSettings() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const handleThemeChange = useCallback((theme: 'auto' | 'light' | 'dark') => {
    // 外置皮肤的明暗由 appearance 定死:切主题 = 退出皮肤回内置
    clearCustomTheme();
    const newConfig = { ...useAppStore.getState().config, theme, customThemeId: undefined };
    setConfig(newConfig);
    applyTheme(theme);
    updateAllTerminalThemes(newConfig.terminalFollowTheme ?? true);
    saveConfigToDisk(newConfig);
  }, [setConfig]);

  const handleSkinChange = useCallback((skin: 'none' | 'blueprint' | 'fluent2') => {
    clearCustomTheme();
    const newConfig = {
      ...useAppStore.getState().config,
      skin,
      customThemeId: undefined,
    };
    setConfig(newConfig);
    applyTheme(newConfig.theme ?? 'auto');
    updateAllTerminalThemes(newConfig.terminalFollowTheme);
    saveConfigToDisk(newConfig);
  }, [setConfig]);

  const handleTerminalFollowThemeChange = useCallback((follow: boolean) => {
    const newConfig = { ...useAppStore.getState().config, terminalFollowTheme: follow };
    setConfig(newConfig);
    updateAllTerminalThemes(follow);
    saveConfigToDisk(newConfig);
  }, [setConfig]);

  return (
    <div className="space-y-6">
      <Section title={t("settings.appearance.language")}>
        <SettingRow title={t("settings.appearance.languageLabel")}>
          <LanguageToggle />
        </SettingRow>
      </Section>

      <Section title={t("settings.appearance.theme")}>
        <ChoiceGroup
          value={(config.customThemeId ? '' : config.theme) as typeof config.theme}
          options={[
            { value: 'dark', label: t("settings.appearance.themeDark") },
            { value: 'light', label: t("settings.appearance.themeLight") },
            { value: 'auto', label: t("settings.appearance.themeAuto") },
          ]}
          onChange={handleThemeChange}
        />
        <ToggleRow
          title={t("settings.appearance.terminalFollowTheme")}
          desc={t("settings.appearance.terminalFollowThemeDesc")}
          checked={config.terminalFollowTheme}
          onChange={handleTerminalFollowThemeChange}
        />
      </Section>

      <Section title={t("settings.appearance.skin")}>
        <ChoiceGroup
          value={(config.customThemeId ? '' : config.skin) as typeof config.skin}
          options={[
            { value: 'none', label: t("settings.appearance.skinNone") },
            { value: 'blueprint', label: t("settings.appearance.skinBlueprint") },
            { value: 'fluent2', label: 'Fluent 2' },
          ]}
          onChange={handleSkinChange}
        />
        <Hint>{t("settings.appearance.skinDesc")}</Hint>
      </Section>

      <CustomThemePacksSection />
    </div>
  );
}

// ─── FontSettings（外观 › 字体）───

function FontSettings() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const patchConfig = useConfigPatch();

  const handleUiFontSizeChange = useCallback((size: number) => {
    const newConfig = { ...useAppStore.getState().config, uiFontSize: size };
    setConfig(newConfig);
    document.documentElement.style.fontSize = `${size}px`;
    saveConfigToDisk(newConfig);
  }, [setConfig]);

  const handleUiFontFamilyChange = useCallback((value: string) => {
    const trimmed = value.trim();
    const newConfig = {
      ...useAppStore.getState().config,
      uiFontFamily: trimmed || undefined,
    };
    setConfig(newConfig);
    applyUiFontFamily(trimmed || undefined);
    saveConfigToDisk(newConfig);
  }, [setConfig]);

  const terminalLigaturesEnabled = config.terminalLigatures ?? false;

  return (
    <div className="space-y-6">
      <Section title={t("settings.font.fontSize")}>
        <FontSizeSlider
          label={t("settings.font.uiFontSize")}
          value={config.uiFontSize ?? 13}
          min={10}
          max={20}
          onChange={handleUiFontSizeChange}
        />
        <FontSizeSlider
          label={t("settings.font.terminalFontSize")}
          value={config.terminalFontSize ?? 14}
          min={10}
          max={24}
          onChange={(v) => patchConfig({ terminalFontSize: v })}
        />
        <Hint>{t("settings.font.fontSizeFooter")}</Hint>
      </Section>

      <Section title={t("settings.font.font")}>
        <FontFamilyInput
          label={t("settings.font.uiFont")}
          value={config.uiFontFamily ?? ''}
          placeholder="'DM Sans', system-ui, sans-serif"
          onChange={handleUiFontFamilyChange}
        />
        <FontFamilyInput
          label={t("settings.font.terminalFont")}
          value={config.terminalFontFamily ?? ''}
          placeholder={DEFAULT_TERMINAL_FONT_FAMILY}
          onChange={(v) => patchConfig({ terminalFontFamily: v.trim() || undefined })}
        />
        <Hint>
          {t("settings.font.fontFamilyFooterPrefix")}<span className="font-mono">'JetBrainsMono Nerd Font', monospace</span>{t("settings.font.fontFamilyFooterSuffix")}
        </Hint>
      </Section>

      <Section title={t("settings.font.ligatures")}>
        <ToggleRow
          title={t("settings.font.ligaturesTitle")}
          desc={
            <>
              {t("settings.font.ligaturesDescPrefix")}<span className="font-mono">==</span> <span className="font-mono">=&gt;</span> <span className="font-mono">!=</span> <span className="font-mono">-&gt;</span>{t("settings.font.ligaturesDescSuffix")}
            </>
          }
          checked={terminalLigaturesEnabled}
          onChange={(v) => patchConfig({ terminalLigatures: v })}
        />
      </Section>
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

// ─── AiHookSettings（AI › Hook 事件）───

function AiHookSettings() {
  const t = useT();
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
    grok: { files: { file: string; content: string; note?: string }[] };
  } | null>(null);
  const [showSnippet, setShowSnippet] = useState(false);
  const [snippetTab, setSnippetTab] = useState<'claude' | 'codex' | 'grok'>('claude');

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
      await saveConfigToDisk(newConfig);
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
    } catch {
      setSnippetData(null);
      setShowSnippet(true);
    }
  }, [showSnippet]);

  return (
    <div className="space-y-2">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        {t("settings.aiHook.title")}
      </div>

      <ToggleRow
        title={t("settings.aiHook.enableHook")}
        desc={t("settings.aiHook.enableHookDesc")}
        checked={hookEnabled}
        onChange={handleToggleHook}
        busy={toggling}
      />

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
              {t("settings.aiHook.serverLabel")} {hookStatus?.running ? t("settings.aiHook.serverRunning", { port: hookStatus.port }) : t("settings.aiHook.serverStopped")}
            </span>
          </div>
          <div className="text-sm text-[var(--text-muted)]">
            {t("settings.aiHook.serverDesc")}
          </div>
        </div>

        <div className="flex gap-2">
          <button
            className="flex-1 py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base hover:opacity-90 transition-opacity disabled:opacity-50"
            onClick={handleRegister}
            disabled={registering}
          >
            {registering ? t("settings.aiHook.registering") : t("settings.aiHook.register")}
          </button>
          <button
            className="flex-1 py-2 bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] text-base hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50"
            onClick={handleUnregister}
            disabled={unregistering}
          >
            {unregistering ? t("settings.aiHook.unregistering") : t("settings.aiHook.unregister")}
          </button>
        </div>

        <button
          className="w-full py-2 text-base text-[var(--text-muted)] hover:text-[var(--accent)] transition-colors"
          onClick={handleShowSnippet}
        >
          {showSnippet ? t("settings.aiHook.collapseSnippet") : t("settings.aiHook.showSnippet")}
        </button>

        {showSnippet && snippetData && (
          <div className="rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-default)] overflow-hidden">
            <div className="flex border-b border-[var(--border-subtle)]">
              {(['claude', 'codex', 'grok'] as const).map((tab) => (
                <button
                  key={tab}
                  className={`flex-1 py-1.5 text-sm transition-colors ${
                    snippetTab === tab
                      ? 'text-[var(--accent)] border-b-2 border-[var(--accent)]'
                      : 'text-[var(--text-muted)] hover:text-[var(--text-secondary)]'
                  }`}
                  onClick={() => setSnippetTab(tab)}
                >
                  {tab === 'claude' ? 'Claude Code' : tab === 'codex' ? 'Codex' : 'Grok'}
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
                (snippetTab === 'codex' ? snippetData.codex : snippetData.grok).files.map((f, i) => (
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

        <Hint>{t("settings.aiHook.footer")}</Hint>
      </div>
    </div>
  );
}

// ─── AiNotificationSettings（AI › 通知提醒）───

function AiNotificationSettings() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const patchConfig = useConfigPatch();

  const handleSoundPathChange = useCallback(async () => {
    const selected = await openDialog({
      title: t("settings.aiNotification.soundDialogTitle"),
      multiple: false,
      directory: false,
      filters: [{ name: t("settings.aiNotification.audioFilter"), extensions: ['mp3', 'wav', 'ogg', 'flac', 'aac', 'm4a'] }],
    });
    if (typeof selected === 'string' && selected.trim()) {
      patchConfig({ aiCompletionSoundPath: selected });
    }
  }, [patchConfig, t]);

  return (
    <div className="space-y-6">
      <Section title={t("settings.aiNotification.method")}>
        <ToggleRow
          title={t("settings.aiNotification.popup")}
          desc={t("settings.aiNotification.popupDesc")}
          checked={config.aiCompletionPopup}
          onChange={(v) => patchConfig({ aiCompletionPopup: v })}
        />
        <ToggleRow
          title={t("settings.aiNotification.taskbarFlash")}
          desc={t("settings.aiNotification.taskbarFlashDesc")}
          checked={config.aiCompletionTaskbarFlash}
          onChange={(v) => patchConfig({ aiCompletionTaskbarFlash: v })}
        />
        <ToggleRow
          title={t("settings.aiNotification.sound")}
          desc={t("settings.aiNotification.soundDesc")}
          checked={config.aiCompletionSound}
          onChange={(v) => patchConfig({ aiCompletionSound: v })}
        />

        <SettingRow
          title={t("settings.aiNotification.customSound")}
          desc={
            <span className="font-mono block truncate">
              {config.aiCompletionSoundPath || t("settings.aiNotification.defaultSound")}
            </span>
          }
          disabled={!config.aiCompletionSound}
        >
          <div className="flex items-center gap-1">
            <button
              className="px-2.5 py-1 text-sm bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
              onClick={() => playNotificationSound(config.aiCompletionSoundPath)}
              title={t("settings.aiNotification.preview")}
            >
              {t("settings.aiNotification.preview")}
            </button>
            <button
              className="px-2.5 py-1 text-sm bg-[var(--bg-elevated)] text-[var(--text-secondary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
              onClick={handleSoundPathChange}
            >
              {t("settings.aiNotification.chooseFile")}
            </button>
            {config.aiCompletionSoundPath && (
              <button
                className="px-2.5 py-1 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors"
                onClick={() => patchConfig({ aiCompletionSoundPath: undefined })}
              >
                {t("settings.aiNotification.clear")}
              </button>
            )}
          </div>
        </SettingRow>

        <Hint>{t("settings.aiNotification.footer")}</Hint>
      </Section>

      <Section title={t("settings.aiNotification.trigger")}>
        <ToggleRow
          title={t("settings.aiNotification.attention")}
          desc={t("settings.aiNotification.attentionDesc")}
          checked={config.aiAttentionNotify}
          onChange={(v) => patchConfig({ aiAttentionNotify: v })}
        />
        <Hint>{t("settings.aiNotification.attentionFooter")}</Hint>
      </Section>
    </div>
  );
}

// ─── SystemSettings（系统 › 常规）───

function SystemSettings() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const patchConfig = useConfigPatch();

  const trayEnabled = config.trayStatusEnabled ?? true;
  const trayClickFocus = config.trayClickFocus ?? true;
  const savedTrayMax = config.trayMaxProjects ?? 5;
  // 缺省开启:保持旧行为,老配置升级上来不改变启动表现
  const aiAutoResume = config.aiAutoResume ?? true;

  return (
    <div className="space-y-6">
      <Section title={t("settings.system.trayGroup")}>
        <ToggleRow
          title={t("settings.system.trayStatusTitle")}
          desc={t("settings.system.trayStatusDesc")}
          checked={trayEnabled}
          onChange={(v) => patchConfig({ trayStatusEnabled: v })}
        />
        {trayEnabled && (
          <>
            <ToggleRow
              title={t("settings.system.trayClickFocusTitle")}
              desc={t("settings.system.trayClickFocusDesc")}
              checked={trayClickFocus}
              onChange={(v) => patchConfig({ trayClickFocus: v })}
            />
            <NumberRow
              title={t("settings.system.trayMaxTitle")}
              desc={t("settings.system.trayMaxDesc")}
              value={savedTrayMax}
              min={1}
              max={20}
              onCommit={(v) => patchConfig({ trayMaxProjects: v })}
            />
          </>
        )}
      </Section>

      <Section title={t("settings.system.startupGroup")}>
        <ToggleRow
          title={t("settings.system.aiAutoResumeTitle")}
          desc={t("settings.system.aiAutoResumeDesc")}
          checked={aiAutoResume}
          onChange={(v) => patchConfig({ aiAutoResume: v })}
        />
      </Section>
    </div>
  );
}

// ─── EditorSettings（系统 › 外部编辑器）───

function EditorSettings() {
  const t = useT();
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
    await saveConfigToDisk(newConfig);
  }, [setConfig]);

  const handleAddEditor = useCallback(() => {
    if (!newEditorName.trim() || !newEditorCommand.trim()) return;
    const trimmedName = newEditorName.trim();
    if (editors.some((e) => e.name === trimmedName)) {
      alert(t("settings.editor.editorExistsAlert", { name: trimmedName }));
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
  }, [editors, defaultEditorName, newEditorName, newEditorCommand, saveEditors, t]);

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
      alert(t("settings.editor.editorExistsAlert", { name: editor.name }));
      return;
    }
    const wasDefault = oldName === defaultEditorName;
    const updated = editors.map((e, i) => (i === idx ? editor : e));
    setEditors(updated);
    const def = wasDefault ? editor.name : defaultEditorName;
    setDefaultEditorName(def);
    saveEditors(updated, def);
  }, [editors, defaultEditorName, saveEditors, t]);

  const handleSetDefaultEditor = useCallback((name: string) => {
    setDefaultEditorName(name);
    saveEditors(editors, name);
  }, [editors, saveEditors]);

  const handleBrowseEditorPath = useCallback(async (onSelect: (path: string) => void) => {
    const isWindows = navigator.userAgent.includes('Windows');
    const selected = await openDialog({
      title: t("settings.editor.browseDialogTitle"),
      multiple: false,
      directory: false,
      filters: isWindows
        ? [{ name: t("settings.editor.executableFilter"), extensions: ['exe'] }]
        : undefined,
    });
    if (typeof selected === 'string' && selected.trim()) {
      onSelect(selected);
    }
  }, [t]);

  return (
    <div className="space-y-6">
      <Section title={t("settings.editor.externalEditor")}>
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
                placeholder={t("settings.editor.newEditorNamePlaceholder")}
                value={newEditorName}
                onChange={(e) => setNewEditorName(e.target.value)}
                autoFocus
              />
            </div>
            <div className="flex gap-2 items-center">
              <input
                className="flex-[2] bg-[var(--bg-elevated)] text-[var(--text-primary)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-2 py-1 text-base outline-none focus:border-[var(--accent)] font-mono"
                placeholder={t("settings.editor.newEditorCommandPlaceholder")}
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
                {t("settings.common.add")}
              </button>
              <button
                className="px-3 py-1 text-base text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
                onClick={() => setAddingEditor(false)}
              >
                {t("settings.common.cancel")}
              </button>
            </div>
          </div>
        ) : (
          <button
            className="w-full py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all"
            onClick={() => setAddingEditor(true)}
          >
            {t("settings.editor.addEditor")}
          </button>
        )}

        <Hint>{t("settings.editor.editorDefaultHint")}</Hint>
      </Section>
    </div>
  );
}

// ─── AboutSettings（关于页）───

function AboutSettings() {
  const t = useT();
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
      setError(e instanceof Error ? e.message : t("settings.about.checkFailed"));
    } finally {
      setChecking(false);
    }
  }, [currentVersion, t]);

  const hasUpdate = latest && compareVersions(latest.version, currentVersion) > 0;

  return (
    <div className="space-y-6">
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        {t("settings.about.versionInfo")}
      </div>

      {/* 当前版本 */}
      <div className="flex items-center gap-3 px-4 py-3 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
        <span className="text-base text-[var(--text-secondary)]">{t("settings.about.currentVersion")}</span>
        <span className="font-mono text-base text-[var(--accent)]">v{currentVersion}</span>
      </div>

      {/* 检查更新按钮 */}
      <button
        className="w-full py-2.5 border border-[var(--border-default)] rounded-[var(--radius-md)] text-base text-[var(--text-secondary)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all disabled:opacity-50 disabled:cursor-not-allowed"
        onClick={checkUpdate}
        disabled={checking}
      >
        {checking ? t("settings.about.checking") : t("settings.about.checkUpdate")}
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
                <span className="text-base text-[var(--text-primary)]">{t("settings.about.newVersionFound")}</span>
                <span className="font-mono text-base text-[var(--accent)]">{latest.version}</span>
              </div>
              <div className="text-sm text-[var(--text-muted)]">
                {t("settings.about.publishedAt", { date: new Date(latest.publishedAt).toLocaleDateString('zh-CN') })}
              </div>
              <button
                className="w-full py-2 bg-[var(--accent)] text-[var(--bg-base)] rounded-[var(--radius-sm)] text-base font-medium hover:opacity-90 transition-opacity"
                onClick={() => openUrl(latest.url)}
              >
                {t("settings.about.downloadFromGitHub")}
              </button>
            </div>
          ) : (
            <div className="text-base text-[var(--text-secondary)]">
              {t("settings.about.upToDate")}
            </div>
          )}
        </div>
      )}

      <Hint>{t("settings.about.footer")}</Hint>
    </div>
  );
}

// ─── ShortcutsSettings（快捷键页）───

/**
 * 快捷键页整个从 `utils/hotkeys.ts` 的表生成 —— 之前这里是一份手写的静态说明，
 * 与实际监听逻辑各写各的，改了键位忘了改说明就直接漂移。
 *
 * 唯一的动态项是「智能 Ctrl+C/V」：它由设置开关决定是否生效，不在静态表里。
 */
function ShortcutsSettings() {
  const t = useT();
  const smartCopyPaste = useAppStore((s) => s.config.smartCopyPaste ?? false);
  const groups = hotkeyGroups();

  const row = (key: string, desc: string, keys: string) => (
    <div
      key={key}
      className="flex items-center justify-between gap-4 px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]"
    >
      <span className="text-base text-[var(--text-primary)]">{desc}</span>
      <kbd className="kbd flex-shrink-0">{keys}</kbd>
    </div>
  );

  return (
    <div className="space-y-6">
      {groups.map((group) => (
        <div key={group.groupKey}>
          <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
            {t(group.groupKey)}
          </div>
          <div className="space-y-1">
            {group.items.map((def) => row(def.id, t(def.descKey), comboLabel(def.combo)))}
            {/* 智能 Ctrl+C/V 开启时才存在，附在「复制粘贴」组末尾 */}
            {group.groupKey === 'settings.shortcuts.clipboard' && smartCopyPaste && (
              <>
                {row('smartCopy', t('settings.shortcuts.copyDesc'), `${MOD_LABEL}+C`)}
                {row('smartPaste', t('settings.shortcuts.pasteToTerminal'), `${MOD_LABEL}+V`)}
              </>
            )}
          </div>
        </div>
      ))}
      <Hint>{t("settings.shortcuts.footer")}</Hint>
    </div>
  );
}

// ─── CustomThemePacksSection（外置主题包，嵌在系统设置的主题/皮肤下方）───

function ThemeCard({
  name,
  colors,
  imageUrl,
  focusX,
  focusY,
  active,
  subtitle,
  onSelect,
  onDelete,
}: {
  name: string;
  colors: { background: string; panel: string; accent: string; text: string; muted?: string };
  imageUrl?: string;
  focusX?: number;
  focusY?: number;
  active: boolean;
  subtitle?: string;
  onSelect: () => void;
  onDelete?: () => void;
}) {
  return (
    <button
      type="button"
      className={`group/card flex flex-col gap-2 p-3 rounded-[var(--radius-md)] text-left transition-all border ${
        active
          ? 'border-[var(--accent)] bg-[var(--accent-subtle)]'
          : 'border-[var(--border-default)] bg-[var(--bg-base)] hover:border-[var(--accent)]'
      }`}
      onClick={onSelect}
    >
      {/* 缩小版实际效果:背景图 + 压暗层 + 迷你侧栏/终端界面 */}
      <div
        className="relative w-full h-24 rounded-[var(--radius-sm)] border border-[var(--border-subtle)] overflow-hidden"
        style={{ backgroundColor: colors.background }}
      >
        {imageUrl && (
          <div
            className="absolute inset-0"
            style={{
              backgroundImage: `url("${imageUrl}")`,
              backgroundSize: 'cover',
              backgroundPosition: `${(focusX ?? 0.5) * 100}% ${(focusY ?? 0.5) * 100}%`,
            }}
          />
        )}
        {/* 压暗层,与真实氛围层同款(35%) */}
        <div
          className="absolute inset-0"
          style={{ backgroundColor: `color-mix(in srgb, ${colors.background} 35%, transparent)` }}
        />
        {/* 迷你侧栏(72% 半透明面板) */}
        <div
          className="absolute left-1.5 top-1.5 bottom-1.5 w-12 rounded-sm px-1.5 py-1 space-y-1"
          style={{ backgroundColor: `color-mix(in srgb, ${colors.panel} 72%, transparent)` }}
        >
          <div className="h-1 w-8 rounded-full" style={{ backgroundColor: colors.accent }} />
          <div className="h-1 w-6 rounded-full opacity-60" style={{ backgroundColor: colors.text }} />
          <div className="h-1 w-7 rounded-full opacity-40" style={{ backgroundColor: colors.text }} />
        </div>
        {/* 迷你终端区(60% 着色 + 提示符) */}
        <div
          className="absolute left-[3.9rem] right-1.5 top-1.5 bottom-1.5 rounded-sm px-1.5 py-1"
          style={{ backgroundColor: `color-mix(in srgb, ${colors.background} 60%, transparent)` }}
        >
          <div className="text-[10px] leading-tight font-mono" style={{ color: colors.accent }}>
            ❯ <span style={{ color: colors.text }}>Aa 字</span>
          </div>
          <div className="mt-0.5 h-1 w-10 rounded-full opacity-50" style={{ backgroundColor: colors.text }} />
        </div>
      </div>
      <div className="flex items-start justify-between gap-2 min-w-0">
        <div className="min-w-0">
          <div className={`text-base truncate ${active ? 'text-[var(--accent)]' : 'text-[var(--text-primary)]'}`}>
            {name}
          </div>
          {subtitle && <div className="text-sm text-[var(--text-muted)] truncate">{subtitle}</div>}
        </div>
        {onDelete && (
          <span
            role="button"
            className="hidden group-hover/card:block px-1 text-sm text-[var(--text-muted)] hover:text-[var(--color-error)] transition-colors flex-shrink-0"
            onClick={(e) => { e.stopPropagation(); onDelete(); }}
          >
            ✕
          </span>
        )}
      </div>
    </button>
  );
}

function CustomThemePacksSection() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const [packs, setPacks] = useState<ThemePackMeta[]>([]);
  const [thumbUrls, setThumbUrls] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  /** 成功提示（生成示例皮肤）；与 error 互斥展示 */
  const [notice, setNotice] = useState<string | null>(null);

  // 缩略图走 asset 探活/base64 兜底通道(asset 协议在部分 WebView 环境不可用,
  // CSS 背景加载失败是静默的),逐个就绪逐个上屏
  useEffect(() => {
    let cancelled = false;
    for (const pack of packs) {
      if (!pack.def.image) continue;
      resolveThemeAssetUrl(pack.dir, pack.themeId, pack.def.image)
        .then((url) => {
          if (!cancelled) setThumbUrls((prev) => ({ ...prev, [pack.themeId]: url }));
        })
        .catch((e) => console.warn(`皮肤 ${pack.themeId} 缩略图加载失败:`, e));
    }
    return () => { cancelled = true; };
  }, [packs]);

  const refresh = useCallback(() => {
    setError(null);
    setNotice(null);
    listThemePacks().then(setPacks).catch((e) => setError(String(e)));
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  const selectCustom = useCallback(async (pack: ThemePackMeta) => {
    setError(null);
    try {
      await loadAndApplyCustomTheme(pack.themeId);
    } catch (e) {
      setError(t('settings.themes.applyFailed', { detail: String(e) }));
      return;
    }
    const cur = useAppStore.getState().config;
    const newConfig = { ...cur, customThemeId: pack.themeId };
    setConfig(newConfig);
    updateAllTerminalThemes(newConfig.terminalFollowTheme ?? true);
    saveConfigToDisk(newConfig);
  }, [setConfig, t]);

  const importPack = useCallback(async () => {
    setError(null);
    const selected = await openDialog({
      title: t('settings.themes.importDialogTitle'),
      directory: true,
      multiple: false,
    });
    if (typeof selected !== 'string' || !selected.trim()) return;
    try {
      const themeId = await invoke<string>('import_theme_pack', { srcDir: selected });
      // 同名覆盖导入换掉的是同一批文件名，缓存不清缩略图还是上一版
      invalidateThemeAssets(themeId);
      refresh();
    } catch (e) {
      setError(String(e));
    }
  }, [refresh, t]);

  const importZip = useCallback(async () => {
    setError(null);
    const selected = await openDialog({
      title: t('settings.themes.importZipDialogTitle'),
      directory: false,
      multiple: false,
      filters: [{ name: 'Zip', extensions: ['zip'] }],
    });
    if (typeof selected !== 'string' || !selected.trim()) return;
    try {
      const themeId = await invoke<string>('import_theme_pack_zip', { zipPath: selected });
      invalidateThemeAssets(themeId);
      refresh();
    } catch (e) {
      setError(String(e));
    }
  }, [refresh, t]);

  const deletePack = useCallback(async (pack: ThemePackMeta) => {
    if (!window.confirm(t('settings.themes.deleteConfirm', { name: pack.def.name }))) return;
    setError(null);
    const cur = useAppStore.getState().config;
    // 先退出该主题（clearCustomTheme 内含 unwatch）再删目录：反过来的话
    // notify 的目录句柄还开着，被删目录在 Windows 上处于 delete-pending，
    // 紧接着重导入同名主题会撞 ERROR_ACCESS_DENIED。
    const wasActive = cur.customThemeId === pack.themeId;
    if (wasActive) clearCustomTheme();
    invalidateThemeAssets(pack.themeId);
    try {
      await invoke('delete_theme_pack', { themeId: pack.themeId });
    } catch (e) {
      setError(String(e));
      // 删失败就把主题装回去，避免用户界面上皮肤没了、目录还在
      if (wasActive) void loadAndApplyCustomTheme(pack.themeId).catch(() => {});
      return;
    }
    if (wasActive) {
      const newConfig = { ...cur, customThemeId: undefined };
      setConfig(newConfig);
      applyTheme(newConfig.theme ?? 'auto');
      updateAllTerminalThemes(newConfig.terminalFollowTheme ?? true);
      saveConfigToDisk(newConfig);
    }
    refresh();
  }, [refresh, setConfig, t]);

  const openThemesDir = useCallback(async () => {
    try {
      const dir = await invoke<string>('get_themes_dir');
      await revealItemInDir(dir);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  // 生成示例皮肤:内容与仓库 docs/theme-pack-example/ 同一份(编译期嵌入),
  // 包内 README.md 就是字段说明。落盘后照常热重载,用户改完保存即见效
  const createExample = useCallback(async () => {
    setError(null);
    setNotice(null);
    try {
      const themeId = await invoke<string>('create_example_theme_pack');
      refresh();
      setNotice(t('settings.themes.exampleCreated', { id: themeId }));
    } catch (e) {
      setError(String(e));
    }
  }, [refresh, t]);

  return (
    <div className="mb-4">
      {/* 五个按钮 + 标题在 680px 弹窗里已经贴边(英文文案更长),允许换行不挤压 */}
      <div className="flex flex-wrap items-center justify-between gap-y-2 mb-2 mt-4">
        <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em]">
          {t('settings.themes.customSection')}
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            className="px-2 py-1 text-sm rounded-[var(--radius-sm)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--text-primary)] transition-all"
            onClick={importPack}
          >
            {t('settings.themes.addPack')}
          </button>
          <button
            className="px-2 py-1 text-sm rounded-[var(--radius-sm)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--text-primary)] transition-all"
            onClick={importZip}
          >
            {t('settings.themes.importZip')}
          </button>
          <button
            className="px-2 py-1 text-sm rounded-[var(--radius-sm)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--text-primary)] transition-all"
            onClick={createExample}
            title={t('settings.themes.createExampleHint')}
          >
            {t('settings.themes.createExample')}
          </button>
          <button
            className="px-2 py-1 text-sm rounded-[var(--radius-sm)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--text-primary)] transition-all"
            onClick={openThemesDir}
          >
            {t('settings.themes.openDir')}
          </button>
          <button
            className="px-2 py-1 text-sm rounded-[var(--radius-sm)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--text-primary)] transition-all"
            onClick={refresh}
          >
            {t('settings.themes.refresh')}
          </button>
        </div>
      </div>
      {packs.length === 0 ? (
        <div className="px-3 py-4 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)] text-sm text-[var(--text-muted)]">
          {t('settings.themes.empty')}
        </div>
      ) : (
        <div className="grid grid-cols-2 gap-2">
          {packs.map((pack) => (
            <ThemeCard
              key={pack.themeId}
              name={pack.def.name}
              subtitle={pack.themeId}
              colors={{
                background: pack.def.colors.background,
                panel: pack.def.colors.panel,
                accent: pack.def.colors.accent,
                text: pack.def.colors.text,
              }}
              imageUrl={thumbUrls[pack.themeId]}
              focusX={pack.def.art?.focusX}
              focusY={pack.def.art?.focusY}
              active={config.customThemeId === pack.themeId}
              onSelect={() => void selectCustom(pack)}
              onDelete={() => void deletePack(pack)}
            />
          ))}
        </div>
      )}
      {error && (
        <div className="mt-2 px-3 py-2 rounded-[var(--radius-sm)] border border-[var(--color-error)] text-sm text-[var(--color-error)]">
          {error}
        </div>
      )}
      {notice && !error && (
        <div className="mt-2 px-3 py-2 rounded-[var(--radius-sm)] border border-[var(--color-success)] text-sm text-[var(--color-success)]">
          {notice}
        </div>
      )}
    </div>
  );
}

// ─── SettingsModal（主弹窗）───

/**
 * 两级侧栏：分组标题（不可点）+ 分页。
 *
 * 平铺 6 页时「系统设置」一页塞了语言、托盘三项、自动续接、主题、皮肤、
 * 终端跟随主题、外部编辑器 —— 找一个开关要滚半页。按主题归组后每页只剩
 * 一屏左右，代价是侧栏多了 4 行标题，比滚动找开关划算。
 */
type MenuGroup = {
  /** 空串 = 无标题分组（快捷键/关于这类顶级项），渲染成一条分隔线 */
  titleKey: string;
  items: { key: SettingsPage; labelKey: string }[];
};

const MENU_GROUPS: MenuGroup[] = [
  {
    titleKey: 'settings.menu.groupTerminal',
    items: [
      { key: 'terminal', labelKey: 'settings.menu.shell' },
      { key: 'clipboard', labelKey: 'settings.menu.clipboard' },
    ],
  },
  {
    titleKey: 'settings.menu.groupAppearance',
    items: [
      { key: 'appearance', labelKey: 'settings.menu.appearance' },
      { key: 'font', labelKey: 'settings.menu.font' },
    ],
  },
  {
    titleKey: 'settings.menu.groupAi',
    items: [
      { key: 'ai-notification', labelKey: 'settings.menu.aiNotification' },
      { key: 'ai-hook', labelKey: 'settings.menu.aiHook' },
    ],
  },
  {
    titleKey: 'settings.menu.groupSystem',
    items: [
      { key: 'system', labelKey: 'settings.menu.general' },
      { key: 'editor', labelKey: 'settings.menu.editor' },
    ],
  },
  {
    titleKey: '',
    items: [
      { key: 'shortcuts', labelKey: 'settings.menu.shortcuts' },
      { key: 'about', labelKey: 'settings.menu.about' },
    ],
  },
];

/** 上下键在扁平化后的分页序列里移动，跳过分组标题。 */
const MENU_ITEMS = MENU_GROUPS.flatMap((g) => g.items);

const PAGES: Record<SettingsPage, () => ReactNode> = {
  terminal: TerminalSettings,
  clipboard: ClipboardSettings,
  appearance: AppearanceSettings,
  font: FontSettings,
  'ai-notification': AiNotificationSettings,
  'ai-hook': AiHookSettings,
  system: SystemSettings,
  editor: EditorSettings,
  shortcuts: ShortcutsSettings,
  about: AboutSettings,
};

export function SettingsModal({ open, onClose, initialPage }: Props) {
  const t = useT();
  const [activePage, setActivePage] = useState<SettingsPage>(initialPage ?? 'terminal');

  useEffect(() => {
    if (open) setActivePage(initialPage ?? 'terminal');
  }, [open, initialPage]);

  const ActivePage = PAGES[activePage] ?? TerminalSettings;

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={t("settings.title")}
      panelClassName="w-[680px] max-h-[80vh]"
    >
      {/* 左右布局 */}
      <div className="flex flex-1 overflow-hidden">
        {/* 左侧菜单 —— tab 语义，可用方向键在分页间移动 */}
        <div
          role="tablist"
          aria-orientation="vertical"
          aria-label={t("settings.title")}
          className="w-[172px] flex-shrink-0 border-r border-[var(--border-subtle)] py-3 px-2 overflow-y-auto"
          onKeyDown={(e) => {
            if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
            e.preventDefault();
            const idx = MENU_ITEMS.findIndex((m) => m.key === activePage);
            const delta = e.key === 'ArrowDown' ? 1 : -1;
            const next = MENU_ITEMS[(idx + delta + MENU_ITEMS.length) % MENU_ITEMS.length];
            setActivePage(next.key);
            requestAnimationFrame(() => {
              (e.currentTarget as HTMLElement)
                ?.querySelector<HTMLElement>(`[data-page="${next.key}"]`)
                ?.focus();
            });
          }}
        >
          {MENU_GROUPS.map((group, gi) => (
            <div key={group.titleKey || `g${gi}`} role="presentation" className="space-y-0.5">
              {group.titleKey ? (
                <div
                  role="presentation"
                  className={`px-3 pb-1 text-sm text-[var(--text-muted)] uppercase tracking-[0.1em] ${gi > 0 ? 'pt-4' : ''}`}
                >
                  {t(group.titleKey)}
                </div>
              ) : (
                <div role="presentation" className="mx-3 my-2 border-t border-[var(--border-subtle)]" />
              )}
              {group.items.map((item) => (
                <button
                  key={item.key}
                  type="button"
                  role="tab"
                  data-page={item.key}
                  aria-selected={activePage === item.key}
                  tabIndex={activePage === item.key ? 0 : -1}
                  className={`w-full flex items-center gap-2 px-3 py-2 rounded-[var(--radius-sm)] cursor-pointer text-base text-left transition-all duration-150 ${
                    activePage === item.key
                      ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                      : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
                  }`}
                  onClick={() => setActivePage(item.key)}
                >
                  {/* 未选中时留位不留色：切页时标签文字不会横向抖一下 */}
                  <span
                    className={`w-0.5 h-4 rounded-full flex-shrink-0 ${
                      activePage === item.key ? 'bg-[var(--accent)]' : 'bg-transparent'
                    }`}
                  />
                  <span className="truncate">{t(item.labelKey)}</span>
                </button>
              ))}
            </div>
          ))}
        </div>

        {/* 右侧内容 */}
        <div className="flex-1 overflow-y-auto px-5 py-4" role="tabpanel">
          <ActivePage />
        </div>
      </div>
    </Modal>
  );
}

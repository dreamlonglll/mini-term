import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId } from '../store';
import { useT } from '../i18n';
import { withMobileRelayDefaults } from '../utils/mobileRelayConfig';
import type { AiLauncher } from '../types';

/**
 * 「移动端」面板里的 AI 启动器管理(docs/adr/0002)。
 *
 * 一条启动器 = 名称 + 可选 shell + 命令,是一条具名的"怎么起一个 AI 会话"。
 * 命令与 shell **只存在于桌面端配置**,移动端只拿到 id 与展示名——这是
 * "移动端不能远程执行任意命令"这条边界的落点,所以这里不提供任何让移动端
 * 自拟命令的入口。
 *
 * 保存时对命令做非阻塞校验提示(首词不是受支持的 AI CLI、或带了 -p/--print 这类
 * 只输出即退出的标志)。这只是把失败从"手机上等 15 秒超时"前移到配置时,
 * **不是安全防线**:防线是"命令只能来自桌面端配置"。
 */

interface DraftState {
  /** 编辑中的启动器 id;'' = 新增 */
  id: string;
  name: string;
  shell: string;
  command: string;
}

const EMPTY_DRAFT: DraftState = { id: '', name: '', shell: '', command: '' };

export function AiLauncherSection() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const launchers = config.mobileRelay?.launchers ?? [];

  const [draft, setDraft] = useState<DraftState | null>(null);
  const [commandWarning, setCommandWarning] = useState(false);

  // 编辑中的命令变化时查一次识别结果:提示随输入实时更新,不阻塞保存
  useEffect(() => {
    const command = draft?.command.trim() ?? '';
    if (!command) {
      setCommandWarning(false);
      return;
    }
    let cancelled = false;
    invoke<boolean>('mobile_relay_check_launcher_command', { command })
      .then((recognized) => {
        if (!cancelled) setCommandWarning(!recognized);
      })
      .catch(() => {
        // 后端不可用(纯前端 dev 模式):不提示,别拿假警告吓人
        if (!cancelled) setCommandWarning(false);
      });
    return () => {
      cancelled = true;
    };
  }, [draft?.command]);

  // 持久化启动器列表并让后端重发一次全量快照(手机侧弹层立即看到新名单)
  const persist = useCallback(async (next: AiLauncher[]) => {
    const cfg = useAppStore.getState().config;
    const newConfig = {
      ...cfg,
      mobileRelay: withMobileRelayDefaults(cfg.mobileRelay, { launchers: next }),
    };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig }).catch(() => {});
    await invoke('mobile_relay_launchers_changed').catch(() => {});
  }, [setConfig]);

  const saveDraft = useCallback(async () => {
    if (!draft) return;
    const name = draft.name.trim();
    const command = draft.command.trim();
    if (!name || !command) return;
    const entry: AiLauncher = {
      id: draft.id || genId(),
      name,
      command,
      ...(draft.shell ? { shell: draft.shell } : {}),
    };
    const next = draft.id
      ? launchers.map((l) => (l.id === draft.id ? entry : l))
      : [...launchers, entry];
    setDraft(null);
    await persist(next);
  }, [draft, launchers, persist]);

  const removeLauncher = useCallback(
    (id: string) => persist(launchers.filter((l) => l.id !== id)),
    [launchers, persist],
  );

  return (
    <div>
      <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
        {t('mobileRelay.launchers.title')}
      </div>
      <p className="text-sm text-[var(--text-muted)] leading-relaxed mb-2">
        {t('mobileRelay.launchers.intro')}
      </p>

      {launchers.length === 0 && !draft && (
        <p className="text-sm text-[var(--color-error)] leading-relaxed mb-2">
          {t('mobileRelay.launchers.empty')}
        </p>
      )}

      <div className="space-y-1.5">
        {launchers.map((launcher) => (
          <div
            key={launcher.id}
            className="flex items-center gap-2 px-3 py-2 rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-subtle)]"
          >
            <div className="flex-1 min-w-0">
              <div className="text-base text-[var(--text-primary)] truncate">{launcher.name}</div>
              <div className="text-sm text-[var(--text-muted)] truncate font-mono">
                {launcher.shell ? `${launcher.shell} › ${launcher.command}` : launcher.command}
              </div>
            </div>
            <button
              className="px-2 py-1 rounded-[var(--radius-sm)] text-sm text-[var(--text-secondary)] hover:text-[var(--accent)] transition-colors"
              onClick={() =>
                setDraft({
                  id: launcher.id,
                  name: launcher.name,
                  shell: launcher.shell ?? '',
                  command: launcher.command,
                })
              }
            >
              {t('mobileRelay.launchers.edit')}
            </button>
            <button
              className="px-2 py-1 rounded-[var(--radius-sm)] text-sm text-[var(--text-secondary)] hover:text-[var(--color-error)] transition-colors"
              onClick={() => void removeLauncher(launcher.id)}
            >
              {t('mobileRelay.launchers.delete')}
            </button>
          </div>
        ))}
      </div>

      {draft ? (
        <div className="mt-2 p-3 rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-default)] space-y-2">
          <input
            type="text"
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            placeholder={t('mobileRelay.launchers.namePlaceholder')}
            className="w-full px-3 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-base text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none"
          />
          <select
            value={draft.shell}
            onChange={(e) => setDraft({ ...draft, shell: e.target.value })}
            className="w-full px-3 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-base text-[var(--text-primary)] focus:border-[var(--accent)] focus:outline-none"
          >
            <option value="">{t('mobileRelay.launchers.defaultShell')}</option>
            {config.availableShells.map((shell) => (
              <option key={shell.name} value={shell.name}>
                {shell.name}
              </option>
            ))}
          </select>
          <input
            type="text"
            value={draft.command}
            onChange={(e) => setDraft({ ...draft, command: e.target.value })}
            placeholder={t('mobileRelay.launchers.commandPlaceholder')}
            spellCheck={false}
            className="w-full px-3 py-1.5 rounded-[var(--radius-sm)] bg-[var(--bg-surface)] border border-[var(--border-default)] text-base font-mono text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none"
          />
          {commandWarning && (
            <p className="text-sm text-[var(--color-ai-working)] leading-relaxed">
              {t('mobileRelay.launchers.commandWarning')}
            </p>
          )}
          <div className="flex gap-2">
            <button
              className="px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)] hover:opacity-90 transition-opacity disabled:opacity-40"
              disabled={!draft.name.trim() || !draft.command.trim()}
              onClick={() => void saveDraft()}
            >
              {t('mobileRelay.launchers.save')}
            </button>
            <button
              className="px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--bg-surface)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] transition-colors"
              onClick={() => setDraft(null)}
            >
              {t('mobileRelay.launchers.cancel')}
            </button>
          </div>
        </div>
      ) : (
        <button
          className="mt-2 px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] transition-colors"
          onClick={() => setDraft({ ...EMPTY_DRAFT })}
        >
          + {t('mobileRelay.launchers.add')}
        </button>
      )}
    </div>
  );
}

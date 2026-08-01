import { useCallback, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { useAppStore, genId, saveConfigToDisk } from '../store';
import { AddRemoteProjectModal } from './AddRemoteProjectModal';
import { hotkeyLabel } from '../utils/hotkeys';
import { useT } from '../i18n';

/**
 * 一个项目都没有时的首屏。
 *
 * 之前这里只有一行「请先在中间栏添加项目」——把用户指到别处，还得自己找按钮。
 * 现在把两个入口（本地目录 / SSH 远程）直接摆在最大的那块空白上，
 * 并顺带交代三个最常用的键位，省得快捷键体系建好了却没人知道。
 */
export function FirstRunGuide() {
  const t = useT();
  const addProject = useAppStore((s) => s.addProject);
  const [remoteOpen, setRemoteOpen] = useState(false);

  const handleAddLocal = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    const path = selected as string;
    const name = path.split(/[/\\]/).pop() || path;
    addProject({ id: genId(), name, path });
    saveConfigToDisk();
  }, [addProject]);

  const primary =
    'px-4 py-2.5 rounded-[var(--radius-md)] text-sm border transition-all duration-200 ' +
    'border-[var(--accent)] bg-[var(--accent-subtle)] text-[var(--accent)] hover:bg-[var(--accent-muted)]';
  const secondary =
    'px-4 py-2.5 rounded-[var(--radius-md)] text-sm border border-dashed transition-all duration-200 ' +
    'border-[var(--border-default)] text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)]';

  const hints: [string, string][] = [
    [hotkeyLabel('newTerminal'), t('settings.shortcuts.newTerminal')],
    [hotkeyLabel('switchProject'), t('settings.shortcuts.switchProject')],
    [hotkeyLabel('terminalSearch'), t('settings.shortcuts.terminalSearch')],
  ];

  return (
    <div className="h-full bg-[var(--bg-terminal)] flex flex-col items-center justify-center gap-6 px-8 text-center">
      <div className="space-y-1.5">
        <div className="text-base font-medium text-[var(--text-primary)]">
          {t('app.firstRun.title')}
        </div>
        <div className="text-sm text-[var(--text-muted)]">{t('app.firstRun.subtitle')}</div>
      </div>

      <div className="flex flex-wrap items-center justify-center gap-2">
        <button type="button" className={primary} onClick={handleAddLocal}>
          {t('app.firstRun.addLocal')}
        </button>
        <button type="button" className={secondary} onClick={() => setRemoteOpen(true)}>
          {t('app.firstRun.addRemote')}
        </button>
      </div>

      <div className="text-xs text-[var(--text-muted)] space-y-1.5 pt-2">
        <div className="opacity-70">{t('app.firstRun.hintsTitle')}</div>
        {hints.map(([keys, desc]) => (
          <div key={desc} className="flex items-center justify-center gap-2">
            <kbd className="kbd">{keys}</kbd>
            <span>{desc}</span>
          </div>
        ))}
      </div>

      <AddRemoteProjectModal open={remoteOpen} onClose={() => setRemoteOpen(false)} />
    </div>
  );
}

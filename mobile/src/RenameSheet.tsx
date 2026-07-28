import { useState } from 'react';
import { renamePane } from './relay';
import { useT } from './i18n';

/**
 * 重命名会话的弹层。
 *
 * 没有"保存中"状态:改名不回执,新名字由桌面端随结构增量推回来——弹层这时早关了,
 * 用户看到的是列表/标题上名字自己变过来。留空 = 恢复默认名(桌面端回落 shell 名)。
 */
export function RenameSheet({
  paneId,
  current,
  onClose,
}: {
  paneId: string;
  /** 当前展示名,预填进输入框 */
  current: string;
  onClose: () => void;
}) {
  const t = useT();
  const [value, setValue] = useState(current);

  const submit = () => {
    renamePane(paneId, value);
    onClose();
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet" onClick={(e) => e.stopPropagation()}>
        <div className="sheet-header">
          <span className="sheet-title">{t('sessions.rename.title')}</span>
          <button className="sheet-close" onClick={onClose}>
            {t('sessions.rename.cancel')}
          </button>
        </div>
        <div className="rename-field">
          <input
            className="rename-input"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder={t('sessions.rename.placeholder')}
            // 与桌面端 MAX_PANE_TITLE_CHARS 对齐:超长会把同组其它 tab 挤出可视区
            maxLength={64}
            autoFocus
            onKeyDown={(e) => {
              if (e.key === 'Enter') submit();
            }}
          />
          <div className="rename-hint">{t('sessions.rename.hint')}</div>
          <button className="rename-confirm" onClick={submit}>
            {t('sessions.rename.confirm')}
          </button>
        </div>
      </div>
    </div>
  );
}

import { useT } from '../i18n';

/** 「AI 已完成、还没去看」的标记。文案与 ProjectSwitcher 里那个共用一个 key。 */
export function DoneTag() {
  const t = useT();
  return <span className="done-tag">{t('panels.done')}</span>;
}

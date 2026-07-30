/**
 * PWA 翻译字典聚合入口,与桌面端 src/i18n 同模式:
 * 每个命名空间一个文件,`t('<ns>.<key>')` 访问。
 */
import { pair } from './pair';
import { sessions } from './sessions';
import { mirror } from './mirror';
import { start } from './start';

type Dict = Record<string, unknown>;

export const dicts: { zh: Dict; en: Dict } = {
  zh: {
    pair: pair.zh,
    sessions: sessions.zh,
    mirror: mirror.zh,
    start: start.zh,
  },
  en: {
    pair: pair.en,
    sessions: sessions.en,
    mirror: mirror.en,
    start: start.en,
  },
};

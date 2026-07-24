/**
 * PWA 翻译字典聚合入口,与桌面端 src/i18n 同模式:
 * 每个命名空间一个文件,`t('<ns>.<key>')` 访问。
 */
import { pair } from './pair';

type Dict = Record<string, unknown>;

export const dicts: { zh: Dict; en: Dict } = {
  zh: {
    pair: pair.zh,
  },
  en: {
    pair: pair.en,
  },
};

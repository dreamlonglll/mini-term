/**
 * 文件/文件夹类型图标薄封装(@baybreezy/file-extension-icon,Material Icon Theme)。
 *
 * 动态 import() 懒加载:全量图标数据(gzip 约 1.2MB)切独立 chunk,主 bundle 零增量;
 * 加载完成前 resolveFileIcon 返回 null,调用方回退现有手绘符号 —— 加载失败不影响功能。
 * 若将来换库/自定义主题包覆盖图标,只改这一个文件。
 */

type FileIconModule = typeof import('@baybreezy/file-extension-icon');

let mod: FileIconModule | null = null;
let loading: Promise<void> | null = null;

export function ensureFileIcons(): Promise<void> {
  loading ??= import('@baybreezy/file-extension-icon')
    .then((m) => {
      mod = m;
    })
    .catch(() => {
      // 加载失败(如懒 chunk 拉取异常):清掉 promise 允许下次重试
      loading = null;
    });
  return loading;
}

export function fileIconsReady(): boolean {
  return mod !== null;
}

// 库的 getMaterial*Icon 每次调用都对 SVG 源码现做 btoa,无内部缓存;
// TreeNode 未 memo、每次树刷新都会对全部可见行重查,必须在这层缓存住。
// key 数量与项目内出现过的文件名种类同阶,设上限防止极端仓库无界增长。
const iconCache = new Map<string, string>();
const ICON_CACHE_MAX = 10000;

/** 返回 base64 SVG data URI;未就绪返回 null(回退通用符号)。 */
export function resolveFileIcon(name: string, isDir: boolean, isOpen = false): string | null {
  if (!mod) return null;
  const key = `${isDir ? (isOpen ? 'D' : 'd') : 'f'}|${name}`;
  let uri = iconCache.get(key);
  if (uri === undefined) {
    if (iconCache.size >= ICON_CACHE_MAX) iconCache.clear();
    uri = isDir ? mod.getMaterialFolderIcon(name, isOpen) : mod.getMaterialFileIcon(name);
    iconCache.set(key, uri);
  }
  return uri;
}

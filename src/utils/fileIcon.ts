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

/** 返回 base64 SVG data URI;未就绪返回 null(回退通用符号)。 */
export function resolveFileIcon(name: string, isDir: boolean, isOpen = false): string | null {
  if (!mod) return null;
  return isDir ? mod.getMaterialFolderIcon(name, isOpen) : mod.getMaterialFileIcon(name);
}

/**
 * 判断路径是否为 WSL UNC 形式(WSL 根项目判定 / Rust 端 WSL override 分支口径)。
 *
 * 与后端 `mt_core::parse_wsl_unc` 保持一致 — 必须覆盖:
 *   - `\\wsl$\<distro>\...`
 *   - `\\wsl.localhost\<distro>\...`
 *   - `\\?\UNC\wsl$\<distro>\...`(Rust canonicalize 在 UNC 上的输出形式)
 *   - `\\?\UNC\wsl.localhost\<distro>\...`
 * host 名按大小写不敏感匹配(`WSL$` / `Wsl.LocalHost` 也能识别)。
 */
export function isWslPath(path: string): boolean {
  // 先剥 verbatim 前缀 `\\?\UNC\`,剥不掉再尝试普通 `\\`
  const afterPrefix = path.startsWith('\\\\?\\UNC\\')
    ? path.slice('\\\\?\\UNC\\'.length)
    : path.startsWith('\\\\')
      ? path.slice(2)
      : null;
  if (afterPrefix === null) return false;
  const sep = afterPrefix.indexOf('\\');
  if (sep <= 0) return false;
  const host = afterPrefix.slice(0, sep).toLowerCase();
  return host === 'wsl$' || host === 'wsl.localhost';
}

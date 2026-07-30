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

/**
 * Windows 盘符路径 → WSL 内可访问路径(`C:\a\b.png` → `/mnt/c/a/b.png`)。
 *
 * 用途:粘贴时把本机临时文件(剪贴板图片 / 长文本转存)的路径粘进 WSL 终端。
 * 直接粘 `C:\...` 的话 WSL 里的 agent 打不开 —— 文件本身经 `/mnt` 自动挂载
 * 是能读到的,缺的只是路径形式。
 *
 * 只处理盘符路径(含 `\\?\C:\` verbatim 前缀)。UNC / 已是 POSIX 形式的路径
 * 返回 null,调用方按原样粘贴。
 *
 * 已知边界:`/mnt` 是 WSL automount 的默认挂载点,用户在 `/etc/wsl.conf` 里
 * 改过 `[automount] root=` 时不成立。失败表现是「文件不存在」,不会误写。
 */
export function windowsPathToWsl(path: string): string | null {
  const stripped = path.startsWith('\\\\?\\') ? path.slice(4) : path;
  const m = /^([A-Za-z]):[\\/](.*)$/.exec(stripped);
  if (!m) return null;
  return `/mnt/${m[1].toLowerCase()}/${m[2].replace(/\\/g, '/')}`;
}

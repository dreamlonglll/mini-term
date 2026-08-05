export const isMac = /Mac|iPod|iPhone|iPad/.test(navigator.platform);
export const isWindows = /Win/.test(navigator.platform);
/** 既不是 mac 也不是 Windows 一律按 Linux 处理（BSD 等同 Linux 的窗口习惯） */
export const isLinux = !isMac && !isWindows;
export const MOD_LABEL = isMac ? '⌘' : 'Ctrl';

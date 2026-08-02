import { useEffect, useState } from 'react';
import { ensureFileIcons, fileIconsReady } from '../utils/fileIcon';

/** 触发文件图标懒加载,就绪时驱动一次重渲染。 */
export function useFileIcons(): boolean {
  const [ready, setReady] = useState(fileIconsReady);
  useEffect(() => {
    if (ready) return;
    let alive = true;
    ensureFileIcons().then(() => {
      if (alive && fileIconsReady()) setReady(true);
    });
    return () => {
      alive = false;
    };
  }, [ready]);
  return ready;
}

export interface MenuItem {
  label: string;
  danger?: boolean;
  disabled?: boolean;
  onClick?: () => void;
  /** 存在时该项为子菜单父项，悬停展开 submenu，自身点击不触发动作 */
  submenu?: MenuEntry[];
}

export interface MenuSeparator {
  separator: true;
}

/** 不可交互的分组标题 */
export interface MenuHeader {
  header: string;
}

export type MenuEntry = MenuItem | MenuSeparator | MenuHeader;

// 模块级变量:追踪当前活跃菜单的 cleanup 函数。
// 通过 `currentCleanup === cleanup` 同时承担"是否仍是当前菜单"与"是否已被清理"两个判断,
// 避免额外的 cleanedUp 布尔标志。
let currentCleanup: (() => void) | null = null;

export function showContextMenu(x: number, y: number, items: MenuEntry[]) {
  // 先关闭上一个菜单(DOM + document listener 一并清理)
  if (currentCleanup) {
    currentCleanup();
  }

  // 已打开的子菜单元素(独立挂在 body 上,便于溢出处理与统一清理)
  const submenus: HTMLElement[] = [];
  const closeSubmenus = () => {
    while (submenus.length) {
      submenus.pop()!.remove();
    }
  };

  // rootMenu 在 buildMenu 之后才赋值;cleanup 仅在之后被调用,闭包引用安全
  let rootMenu: HTMLElement;

  const cleanup = () => {
    // 已被替换或清理 → 幂等返回
    if (currentCleanup !== cleanup) return;
    currentCleanup = null;
    closeSubmenus();
    rootMenu.remove();
    document.removeEventListener('click', cleanup);
    document.removeEventListener('keydown', onKey);
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === 'Escape') cleanup();
  };

  // 把菜单放进视口:右/下溢出则向反方向翻转
  const placeInViewport = (menu: HTMLElement, px: number, py: number) => {
    const margin = 4;
    const { offsetWidth: w, offsetHeight: h } = menu;
    let fx = px;
    let fy = py;
    if (px + w + margin > window.innerWidth) fx = Math.max(margin, px - w);
    if (py + h + margin > window.innerHeight) fy = Math.max(margin, py - h);
    menu.style.left = `${fx}px`;
    menu.style.top = `${fy}px`;
    menu.style.visibility = '';
  };

  // 递归构建菜单 DOM。isRoot 决定是否在悬停时管理子菜单(子菜单内的项不再嵌套)
  const buildMenu = (entries: MenuEntry[], isRoot: boolean): HTMLElement => {
    const menu = document.createElement('div');
    menu.className = 'fixed ctx-menu text-xs';
    menu.style.visibility = 'hidden';

    entries.forEach((entry) => {
      if ('separator' in entry) {
        const sep = document.createElement('div');
        sep.className = 'ctx-menu-sep';
        menu.appendChild(sep);
        return;
      }
      if ('header' in entry) {
        const head = document.createElement('div');
        head.className = 'ctx-menu-header';
        head.textContent = entry.header;
        menu.appendChild(head);
        return;
      }
      const item = document.createElement('div');
      const classes = ['ctx-menu-item'];
      if (entry.danger) classes.push('danger');
      if (entry.disabled) classes.push('disabled');
      if (entry.submenu) classes.push('has-submenu');
      item.className = classes.join(' ');
      item.textContent = entry.label;

      if (entry.submenu && !entry.disabled) {
        const sub = entry.submenu;
        item.onmouseenter = () => {
          closeSubmenus();
          const child = buildMenu(sub, false);
          document.body.appendChild(child);
          const rect = item.getBoundingClientRect();
          // 紧贴父项右缘展开,避免与父项之间出现鼠标可穿过的间隙
          placeInViewport(child, rect.right - 2, rect.top - 4);
          submenus.push(child);
        };
        // 点击子菜单父项本身不触发动作、也不关闭菜单
        item.onclick = (e) => e.stopPropagation();
      } else {
        // 悬停根菜单的普通项时收起已展开的子菜单
        if (isRoot) {
          item.onmouseenter = () => closeSubmenus();
        }
        item.onclick = () => {
          if (entry.disabled) return;
          entry.onClick?.();
          cleanup();
        };
      }
      menu.appendChild(item);
    });
    return menu;
  };

  rootMenu = buildMenu(items, true);
  document.body.appendChild(rootMenu);
  placeInViewport(rootMenu, x, y);

  currentCleanup = cleanup;

  // 延迟一帧注册,避免当前鼠标事件冒泡到 document 立刻触发 cleanup
  setTimeout(() => {
    // 如果在排队期间已被替换或清理,不再注册
    if (currentCleanup !== cleanup) return;
    document.addEventListener('click', cleanup);
    document.addEventListener('keydown', onKey);
  }, 0);
}

import { isTopOverlay, popOverlay, pushOverlay } from './overlayStack';

export interface MenuItem {
  label: string;
  danger?: boolean;
  disabled?: boolean;
  /** 右侧显示的快捷键提示（如 `Ctrl+Shift+D`），仅展示不参与匹配 */
  shortcut?: string;
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

  // 已打开的子菜单元素(独立挂在 body 上,便于溢出处理与统一清理)。
  // 数组下标 = 层级 - 1:submenus[0] 是根菜单展开的子菜单,submenus[1] 是它再展开的孙菜单……
  const submenus: HTMLElement[] = [];
  // 与 submenus 同步入栈:记录每层子菜单是从哪一项展开的(ArrowLeft 收起时把焦点还给它)
  const submenuOwners: HTMLElement[] = [];
  /** 关掉层级深于 level 的子菜单(level 0 = 根菜单,即全部关掉) */
  const closeSubmenusFrom = (level: number) => {
    while (submenus.length > level) {
      submenus.pop()!.remove();
      submenuOwners.pop();
    }
  };
  const closeSubmenus = () => closeSubmenusFrom(0);

  // rootMenu 在 buildMenu 之后才赋值;cleanup 仅在之后被调用,闭包引用安全
  let rootMenu: HTMLElement;
  // 打开菜单前的焦点。只在**键盘路径**下还回去 —— 无条件还原会出两种事故:
  //   · 点菜单外的另一个终端来关菜单 → 焦点被抢回原来那个,接着敲的命令进错终端
  //   · 菜单项打开了输入类弹窗(重命名)→ 弹窗同步 focus 了输入框,
  //     随后 cleanup 又把焦点抢走,用户输入全落进背后的终端
  const prevFocus = document.activeElement as HTMLElement | null;
  const overlayId = pushOverlay('menu');

  const cleanup = (opts: { restoreFocus?: boolean } = {}) => {
    // 已被替换或清理 → 幂等返回
    if (currentCleanup !== cleanup) return;
    currentCleanup = null;
    popOverlay(overlayId);
    closeSubmenus();
    rootMenu.remove();
    document.removeEventListener('click', docClick);
    window.removeEventListener('keydown', onKey, true);
    // 焦点仍在菜单内部时才还原(键盘导航路径);鼠标点到别处关闭时不动焦点
    if (opts.restoreFocus) prevFocus?.focus?.();
  };
  /** 点击文档任意处关闭 —— 此时焦点已由那次点击自行落位,不要抢 */
  const docClick = () => cleanup();

  /**
   * 当前键盘作用域里可聚焦的项 —— 取**焦点所在的最深一层菜单**。
   *
   * 子菜单是悬停展开的,鼠标移开并不会收起它。若无条件以「最后展开的子菜单」
   * 为作用域,用户鼠标划过子菜单父项后再按 ↓,焦点会直接跳进子菜单 ——
   * 而他看着的还是根菜单。所以只有焦点确实落在某层里时才用它当作用域。
   */
  const focusableItems = (): HTMLElement[] => {
    let scope: HTMLElement = rootMenu;
    for (const sub of submenus) {
      if (sub.contains(document.activeElement)) scope = sub;
    }
    return Array.from(
      scope.querySelectorAll<HTMLElement>('.ctx-menu-item:not(.disabled)'),
    );
  };

  const moveFocus = (delta: 1 | -1) => {
    const list = focusableItems();
    if (list.length === 0) return;
    const idx = list.indexOf(document.activeElement as HTMLElement);
    // 未聚焦任何项时：↓ 从头开始，↑ 从尾开始
    const next = idx < 0
      ? (delta === 1 ? 0 : list.length - 1)
      : (idx + delta + list.length) % list.length;
    list[next].focus();
  };

  const onKey = (e: KeyboardEvent) => {
    // 只有栈顶覆盖物响应(菜单上面又开了弹窗时,按键归弹窗)
    if (!isTopOverlay(overlayId)) return;
    switch (e.key) {
      case 'Escape':
        e.preventDefault();
        e.stopPropagation();
        cleanup({ restoreFocus: true });
        break;
      case 'ArrowDown':
        e.preventDefault();
        e.stopPropagation();
        moveFocus(1);
        break;
      case 'ArrowUp':
        e.preventDefault();
        e.stopPropagation();
        moveFocus(-1);
        break;
      case 'ArrowRight': {
        // 在子菜单父项上展开并进入
        const active = document.activeElement as HTMLElement | null;
        if (active?.classList.contains('has-submenu')) {
          e.preventDefault();
          e.stopPropagation();
          active.dispatchEvent(new MouseEvent('mouseenter'));
          // 直接进刚展开的那一层:此刻焦点还在父项上,走 focusableItems() 只会
          // 算出父项所在的菜单,焦点会跳回本层第一项而不是进子菜单
          requestAnimationFrame(() => {
            submenus[submenus.length - 1]
              ?.querySelector<HTMLElement>('.ctx-menu-item:not(.disabled)')
              ?.focus();
          });
        }
        break;
      }
      case 'ArrowLeft':
        if (submenus.length > 0) {
          e.preventDefault();
          e.stopPropagation();
          // 只收起最深的一层,还给**展开它的那一项**（而不是第一个 has-submenu:
          // 一个菜单里可能有好几个子菜单入口）
          const owner = submenuOwners[submenus.length - 1];
          closeSubmenusFrom(submenus.length - 1);
          owner?.focus();
        }
        break;
      case 'Enter':
      case ' ': {
        const active = document.activeElement as HTMLElement | null;
        if (active?.classList.contains('ctx-menu-item')) {
          e.preventDefault();
          e.stopPropagation();
          active.click();
        }
        break;
      }
    }
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

  // 递归构建菜单 DOM。level 0 = 根菜单,每深一层 +1 —— 悬停某层的项只收起**比它更深**
  // 的子菜单,自身所在的那层保持展开,子菜单因此可以无限层嵌套(分组树选择要用)
  const buildMenu = (entries: MenuEntry[], level: number): HTMLElement => {
    const menu = document.createElement('div');
    menu.className = 'fixed ctx-menu text-xs';
    menu.setAttribute('role', 'menu');
    menu.style.visibility = 'hidden';

    entries.forEach((entry) => {
      if ('separator' in entry) {
        const sep = document.createElement('div');
        sep.className = 'ctx-menu-sep';
        sep.setAttribute('role', 'separator');
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
      item.setAttribute('role', 'menuitem');
      if (!entry.disabled) item.tabIndex = -1;
      if (entry.disabled) item.setAttribute('aria-disabled', 'true');

      const label = document.createElement('span');
      label.className = 'ctx-menu-label';
      label.textContent = entry.label;
      item.appendChild(label);
      if (entry.shortcut) {
        const kbd = document.createElement('span');
        kbd.className = 'ctx-menu-shortcut';
        kbd.textContent = entry.shortcut;
        item.appendChild(kbd);
      }

      if (entry.submenu && !entry.disabled) {
        const sub = entry.submenu;
        item.setAttribute('aria-haspopup', 'menu');
        item.onmouseenter = () => {
          // 先收掉比本层更深的子菜单(含自己上次展开的那个),本层及祖先原样保留
          closeSubmenusFrom(level);
          const child = buildMenu(sub, level + 1);
          document.body.appendChild(child);
          const rect = item.getBoundingClientRect();
          // 紧贴父项右缘展开,避免与父项之间出现鼠标可穿过的间隙
          placeInViewport(child, rect.right - 2, rect.top - 4);
          submenus.push(child);
          submenuOwners.push(item);
        };
        // 点击子菜单父项本身不触发动作、也不关闭菜单
        item.onclick = (e) => e.stopPropagation();
      } else {
        // 悬停普通项时收起本层展开的子菜单(祖先层不动)
        item.onmouseenter = () => closeSubmenusFrom(level);
        item.onclick = () => {
          if (entry.disabled) return;
          // 先收菜单再执行动作:动作可能同步打开一个输入弹窗并聚焦输入框,
          // 反过来的话 cleanup 会把焦点从那个输入框上抢走
          cleanup();
          entry.onClick?.();
        };
      }
      menu.appendChild(item);
    });
    return menu;
  };

  rootMenu = buildMenu(items, 0);
  document.body.appendChild(rootMenu);
  placeInViewport(rootMenu, x, y);

  currentCleanup = cleanup;

  // 延迟一帧注册,避免当前鼠标事件冒泡到 document 立刻触发 cleanup
  setTimeout(() => {
    // 如果在排队期间已被替换或清理,不再注册
    if (currentCleanup !== cleanup) return;
    document.addEventListener('click', docClick);
    // 挂 window 的 capture:capture 是 window → document → …,
    // 挂 document 会排在 Modal / prompt 的 window 监听之后,Esc 被它们先吃掉
    window.addEventListener('keydown', onKey, true);
  }, 0);
}

interface MenuItem {
  label: string;
  danger?: boolean;
  onClick: () => void;
}

interface MenuSeparator {
  separator: true;
}

type MenuEntry = MenuItem | MenuSeparator;

export function showContextMenu(x: number, y: number, items: MenuEntry[]) {
  // 关闭已有的右键菜单
  document.querySelectorAll('.ctx-menu').forEach((el) => el.remove());

  const menu = document.createElement('div');
  menu.className = 'fixed ctx-menu text-xs';
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;

  items.forEach((entry) => {
    if ('separator' in entry) {
      const sep = document.createElement('div');
      sep.className = 'ctx-menu-sep';
      menu.appendChild(sep);
      return;
    }
    const item = document.createElement('div');
    item.className = entry.danger ? 'ctx-menu-item danger' : 'ctx-menu-item';
    item.textContent = entry.label;
    item.onclick = () => {
      entry.onClick();
      cleanup();
    };
    menu.appendChild(item);
  });

  document.body.appendChild(menu);

  const cleanup = () => {
    menu.remove();
    document.removeEventListener('click', cleanup);
    document.removeEventListener('keydown', onKey);
  };
  const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') cleanup(); };
  setTimeout(() => {
    document.addEventListener('click', cleanup);
    document.addEventListener('keydown', onKey);
  }, 0);
}

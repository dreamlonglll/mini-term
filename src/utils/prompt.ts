import { isTopOverlay, popOverlay, pushOverlay } from "./overlayStack";
import { t } from "../i18n";

/**
 * 命令式弹窗（confirm / alert / prompt），替代 window.* 的同名 API。
 *
 * 三个弹窗此前各写了一遍 DOM 拼装与按键处理，并因此各自带着同一个 bug：
 * `document.addEventListener('keydown', handler)` 只在**按键**路径里 remove，
 * 用户点按钮关闭时监听器留在 document 上 —— 开一次漏一个，之后每次按 Enter
 * 都会多跑一遍已失效的 handler。这里统一成一个 `createDialog`，
 * 清理走同一条路，顺带补上 role/aria、焦点陷阱与关闭后的焦点还原。
 */

interface DialogHandle {
  dialog: HTMLElement;
  /** 关闭弹窗并做全部清理（幂等） */
  close: () => void;
}

const FOCUSABLE = 'button, input, [tabindex]:not([tabindex="-1"])';

function createDialog(
  title: string,
  onKey: (e: KeyboardEvent) => boolean | void,
): DialogHandle {
  const overlay = document.createElement('div');
  overlay.className = 'prompt-overlay';

  const dialog = document.createElement('div');
  dialog.className = 'prompt-dialog';
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', title);

  const titleEl = document.createElement('div');
  titleEl.className = 'prompt-title';
  titleEl.textContent = title;
  dialog.appendChild(titleEl);

  overlay.appendChild(dialog);
  document.body.appendChild(overlay);

  const prevFocus = document.activeElement as HTMLElement | null;
  const overlayId = pushOverlay('prompt');
  let closed = false;

  const keyHandler = (e: KeyboardEvent) => {
    // 只有栈顶才响应：弹窗里再弹 confirm 时，Esc/Enter 归最上面这一层，
    // 否则同一次 Enter 会把叠在一起的两个确认框一起 resolve 掉
    if (closed || !isTopOverlay(overlayId)) return;
    // Tab 环形约束在弹窗内，焦点不跑到背后的界面上
    if (e.key === 'Tab') {
      const items = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE));
      if (items.length === 0) return;
      const first = items[0];
      const last = items[items.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
      return;
    }
    if (onKey(e) !== false) {
      e.stopPropagation();
    }
  };

  const close = () => {
    if (closed) return;
    closed = true;
    popOverlay(overlayId);
    // 无论走按钮、遮罩还是按键，清理都收敛到这一处
    window.removeEventListener('keydown', keyHandler, true);
    overlay.remove();
    prevFocus?.focus?.();
  };

  // 挂在 window 而非 document 的 capture：capture 是 window → document → …，
  // 挂 document 的话会排在 Modal 的 window 监听之后，Esc 就被底下的弹窗先吃掉了
  window.addEventListener('keydown', keyHandler, true);

  return { dialog, close };
}

/** 往弹窗里加一段说明文字。 */
function appendMessage(dialog: HTMLElement, message: string): void {
  const msgEl = document.createElement('div');
  msgEl.className = 'prompt-message';
  msgEl.textContent = message;
  dialog.appendChild(msgEl);
}

/** 往弹窗里加按钮行，返回创建出的按钮（顺序同传入）。 */
function appendButtons(
  dialog: HTMLElement,
  specs: { label: string; kind: 'confirm' | 'cancel' }[],
): HTMLButtonElement[] {
  const row = document.createElement('div');
  row.className = 'prompt-buttons';
  const buttons = specs.map((spec) => {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = `prompt-btn prompt-btn-${spec.kind}`;
    btn.textContent = spec.label;
    row.appendChild(btn);
    return btn;
  });
  dialog.appendChild(row);
  return buttons;
}

/** 自定义 confirm，替代 window.confirm。 */
export function showConfirm(title: string, message: string): Promise<boolean> {
  return new Promise((resolve) => {
    const { dialog, close } = createDialog(title, (e) => {
      if (e.key === 'Enter') { finish(true); return; }
      if (e.key === 'Escape') { finish(false); return; }
      return false;
    });

    appendMessage(dialog, message);
    const [cancelBtn, confirmBtn] = appendButtons(dialog, [
      { label: t('prompt.cancel'), kind: 'cancel' },
      { label: t('prompt.confirm'), kind: 'confirm' },
    ]);

    const finish = (value: boolean) => {
      close();
      resolve(value);
    };

    confirmBtn.onclick = () => finish(true);
    cancelBtn.onclick = () => finish(false);
    // 遮罩点击 = 取消（破坏性操作的确认框，误点应当落在「不做」那边）
    dialog.parentElement!.onclick = (e) => {
      if (e.target === dialog.parentElement) finish(false);
    };
    confirmBtn.focus();
  });
}

/** 自定义 alert，替代 window.alert。 */
export function showAlert(title: string, message: string): Promise<void> {
  return new Promise((resolve) => {
    const { dialog, close } = createDialog(title, (e) => {
      if (e.key === 'Enter' || e.key === 'Escape') { finish(); return; }
      return false;
    });

    appendMessage(dialog, message);
    const [okBtn] = appendButtons(dialog, [{ label: t('prompt.ok'), kind: 'confirm' }]);

    const finish = () => {
      close();
      resolve();
    };

    okBtn.onclick = finish;
    dialog.parentElement!.onclick = (e) => {
      if (e.target === dialog.parentElement) finish();
    };
    okBtn.focus();
  });
}

/** 自定义 prompt，替代 window.prompt；取消返回 null。 */
export function showPrompt(
  title: string,
  placeholder?: string,
  defaultValue?: string,
): Promise<string | null> {
  return new Promise((resolve) => {
    const { dialog, close } = createDialog(title, (e) => {
      // Enter/Escape 挂在 document 上而不是 input 上：焦点移到按钮后这两个键仍然管用
      if (e.key === 'Enter') { finish(input.value); return; }
      if (e.key === 'Escape') { finish(null); return; }
      return false;
    });

    const input = document.createElement('input');
    input.className = 'prompt-input';
    input.placeholder = placeholder ?? '';
    input.spellcheck = false;
    if (defaultValue) input.value = defaultValue;
    dialog.appendChild(input);

    const [cancelBtn, confirmBtn] = appendButtons(dialog, [
      { label: t('prompt.cancel'), kind: 'cancel' },
      { label: t('prompt.confirm'), kind: 'confirm' },
    ]);

    const finish = (value: string | null) => {
      close();
      resolve(value);
    };

    // 注意 input.value 而不是 `input.value || null`：空串是有意义的输入
    // （「清掉自定义名、回落 shell 名」），把它和「取消」都压成 null 的话
    // 重命名过的终端就再也改不回默认名了
    confirmBtn.onclick = () => finish(input.value);
    cancelBtn.onclick = () => finish(null);
    dialog.parentElement!.onclick = (e) => {
      if (e.target === dialog.parentElement) finish(null);
    };
    input.focus();
    // 有默认值时全选：重命名场景下多半是要整个换掉
    if (defaultValue) input.select();
  });
}

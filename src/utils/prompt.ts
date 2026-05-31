import { t } from "../i18n";

/**
 * 自定义 confirm 弹窗，替代 window.confirm
 * 返回 Promise<boolean>
 */
export function showConfirm(title: string, message: string): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement('div');
    overlay.className = 'prompt-overlay';

    const dialog = document.createElement('div');
    dialog.className = 'prompt-dialog';

    const titleEl = document.createElement('div');
    titleEl.className = 'prompt-title';
    titleEl.textContent = title;
    dialog.appendChild(titleEl);

    const msgEl = document.createElement('div');
    msgEl.className = 'prompt-message';
    msgEl.textContent = message;
    dialog.appendChild(msgEl);

    const buttons = document.createElement('div');
    buttons.className = 'prompt-buttons';

    const cancelBtn = document.createElement('button');
    cancelBtn.className = 'prompt-btn prompt-btn-cancel';
    cancelBtn.textContent = t("prompt.cancel");

    const confirmBtn = document.createElement('button');
    confirmBtn.className = 'prompt-btn prompt-btn-confirm';
    confirmBtn.textContent = t("prompt.confirm");

    buttons.appendChild(cancelBtn);
    buttons.appendChild(confirmBtn);
    dialog.appendChild(buttons);
    overlay.appendChild(dialog);
    document.body.appendChild(overlay);

    confirmBtn.focus();

    const cleanup = (value: boolean) => {
      overlay.remove();
      resolve(value);
    };

    confirmBtn.onclick = () => cleanup(true);
    cancelBtn.onclick = () => cleanup(false);
    overlay.onclick = (e) => { if (e.target === overlay) cleanup(false); };
    document.addEventListener('keydown', function handler(e) {
      if (e.key === 'Enter') { cleanup(true); document.removeEventListener('keydown', handler); }
      if (e.key === 'Escape') { cleanup(false); document.removeEventListener('keydown', handler); }
    });
  });
}

/**
 * 自定义信息提示弹窗，替代 window.alert
 * 只有一个「知道了」按钮，返回 Promise<void>
 */
export function showAlert(title: string, message: string): Promise<void> {
  return new Promise((resolve) => {
    const overlay = document.createElement('div');
    overlay.className = 'prompt-overlay';

    const dialog = document.createElement('div');
    dialog.className = 'prompt-dialog';

    const titleEl = document.createElement('div');
    titleEl.className = 'prompt-title';
    titleEl.textContent = title;
    dialog.appendChild(titleEl);

    const msgEl = document.createElement('div');
    msgEl.className = 'prompt-message';
    msgEl.textContent = message;
    dialog.appendChild(msgEl);

    const buttons = document.createElement('div');
    buttons.className = 'prompt-buttons';

    const okBtn = document.createElement('button');
    okBtn.className = 'prompt-btn prompt-btn-confirm';
    okBtn.textContent = t("prompt.ok");

    buttons.appendChild(okBtn);
    dialog.appendChild(buttons);
    overlay.appendChild(dialog);
    document.body.appendChild(overlay);

    okBtn.focus();

    const cleanup = () => {
      overlay.remove();
      resolve();
    };

    okBtn.onclick = cleanup;
    overlay.onclick = (e) => { if (e.target === overlay) cleanup(); };
    document.addEventListener('keydown', function handler(e) {
      if (e.key === 'Enter' || e.key === 'Escape') {
        cleanup();
        document.removeEventListener('keydown', handler);
      }
    });
  });
}

/**
 * 自定义 prompt 弹窗，替代 window.prompt
 * 返回 Promise<string | null>，取消返回 null
 */
export function showPrompt(title: string, placeholder?: string, defaultValue?: string): Promise<string | null> {
  return new Promise((resolve) => {
    // 遮罩层
    const overlay = document.createElement('div');
    overlay.className = 'prompt-overlay';

    // 弹窗
    const dialog = document.createElement('div');
    dialog.className = 'prompt-dialog';

    // 标题
    const titleEl = document.createElement('div');
    titleEl.className = 'prompt-title';
    titleEl.textContent = title;
    dialog.appendChild(titleEl);

    // 输入框
    const input = document.createElement('input');
    input.className = 'prompt-input';
    input.placeholder = placeholder ?? '';
    input.spellcheck = false;
    if (defaultValue) {
      input.value = defaultValue;
    }
    dialog.appendChild(input);

    // 按钮区
    const buttons = document.createElement('div');
    buttons.className = 'prompt-buttons';

    const cancelBtn = document.createElement('button');
    cancelBtn.className = 'prompt-btn prompt-btn-cancel';
    cancelBtn.textContent = t("prompt.cancel");

    const confirmBtn = document.createElement('button');
    confirmBtn.className = 'prompt-btn prompt-btn-confirm';
    confirmBtn.textContent = t("prompt.confirm");

    buttons.appendChild(cancelBtn);
    buttons.appendChild(confirmBtn);
    dialog.appendChild(buttons);
    overlay.appendChild(dialog);
    document.body.appendChild(overlay);

    input.focus();

    const cleanup = (value: string | null) => {
      overlay.remove();
      resolve(value);
    };

    confirmBtn.onclick = () => cleanup(input.value || null);
    cancelBtn.onclick = () => cleanup(null);
    overlay.onclick = (e) => { if (e.target === overlay) cleanup(null); };
    input.onkeydown = (e) => {
      if (e.key === 'Enter') cleanup(input.value || null);
      if (e.key === 'Escape') cleanup(null);
    };
  });
}

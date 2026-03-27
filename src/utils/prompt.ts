/**
 * 自定义 prompt 弹窗，替代 window.prompt
 * 返回 Promise<string | null>，取消返回 null
 */
export function showPrompt(title: string, placeholder?: string): Promise<string | null> {
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
    dialog.appendChild(input);

    // 按钮区
    const buttons = document.createElement('div');
    buttons.className = 'prompt-buttons';

    const cancelBtn = document.createElement('button');
    cancelBtn.className = 'prompt-btn prompt-btn-cancel';
    cancelBtn.textContent = '取消';

    const confirmBtn = document.createElement('button');
    confirmBtn.className = 'prompt-btn prompt-btn-confirm';
    confirmBtn.textContent = '确定';

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

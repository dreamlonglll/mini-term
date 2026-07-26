import { useEffect, useRef, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { isTopOverlay, popOverlay, pushOverlay } from '../utils/overlayStack';
import { useT } from '../i18n';

/**
 * 全应用统一的弹窗外壳。
 *
 * 改造前 11 个 modal 有四套关闭规则（Esc / 点遮罩 各自为政），最常开的「设置」
 * 反而不支持 Esc。统一到这里之后：
 *  - **Esc 一律关**（`closeOnEscape={false}` 只留给「正在跑不可中断的操作」）
 *  - **点遮罩默认关**；含未保存输入的弹窗传 `closeOnOverlay={false}` 防误触，
 *    但仍保留 Esc —— 让用户有一条不用找 ✕ 的退路
 *  - 补齐 `role="dialog"` / `aria-modal`、打开即聚焦、焦点不外泄、关闭还原焦点
 *
 * 嵌套弹窗（如搜索里再开文件预览）靠模块级栈处理：Esc 只关最上面那个。
 */

const FOCUSABLE = [
  'a[href]', 'button:not([disabled])', 'input:not([disabled])',
  'select:not([disabled])', 'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',');

export interface ModalProps {
  open: boolean;
  onClose: () => void;
  /** 顶栏标题；省略时不渲染顶栏（调用方自行安排头部） */
  title?: ReactNode;
  /** 顶栏标题右侧、关闭按钮左侧的额外内容 */
  headerExtra?: ReactNode;
  /** 点击遮罩是否关闭。默认 true；有未保存输入时传 false */
  closeOnOverlay?: boolean;
  /** Esc 是否关闭。默认 true；仅在「正在执行不可中断操作」时传 false */
  closeOnEscape?: boolean;
  /** 面板尺寸相关的类名，如 `w-[640px] max-h-[80vh]` */
  panelClassName?: string;
  /** 垂直对齐：顶部偏上（表单类）或居中（查看器类） */
  align?: 'top' | 'center';
  /** 无 title 时用于无障碍标注的名称 */
  ariaLabel?: string;
  children: ReactNode;
}

export function Modal({
  open,
  onClose,
  title,
  headerExtra,
  closeOnOverlay = true,
  closeOnEscape = true,
  panelClassName = 'w-[520px] max-h-[80vh]',
  align = 'top',
  ariaLabel,
  children,
}: ModalProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const idRef = useRef<number>(0);
  const restoreFocusRef = useRef<HTMLElement | null>(null);

  // 入栈 / 出栈 + 焦点保存还原
  useEffect(() => {
    if (!open) return;
    const id = pushOverlay('modal');
    idRef.current = id;
    restoreFocusRef.current = document.activeElement as HTMLElement | null;

    // 打开即把焦点送进面板：优先第一个输入框，否则面板本身
    const raf = requestAnimationFrame(() => {
      const panel = panelRef.current;
      if (!panel) return;
      const firstInput = panel.querySelector<HTMLElement>('input:not([type="hidden"]):not([disabled]), textarea:not([disabled])');
      (firstInput ?? panel).focus();
    });

    return () => {
      cancelAnimationFrame(raf);
      popOverlay(id);
      // 还原焦点，键盘用户不会被丢回 body
      restoreFocusRef.current?.focus?.();
    };
  }, [open]);

  // Esc 关闭 + 焦点陷阱
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      // 只有栈顶覆盖物响应 —— 栈里也算上命令式弹窗与右键菜单，
      // 所以「弹窗里再弹 confirm」时 Esc 归 confirm，不会误关底下的弹窗
      if (!isTopOverlay(idRef.current)) return;

      if (e.key === 'Escape' && closeOnEscape) {
        e.preventDefault();
        e.stopPropagation();
        onClose();
        return;
      }

      if (e.key !== 'Tab') return;
      const panel = panelRef.current;
      if (!panel) return;
      const items = Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE))
        .filter((el) => el.offsetParent !== null);
      if (items.length === 0) {
        e.preventDefault();
        panel.focus();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      // 环形 Tab：焦点不跑到弹窗外面去
      if (e.shiftKey && (active === first || active === panel)) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus();
      }
    };
    // capture：抢在全局快捷键（同为 capture）之前，弹窗里的 Esc 优先关弹窗
    window.addEventListener('keydown', handler, true);
    return () => window.removeEventListener('keydown', handler, true);
  }, [open, onClose, closeOnEscape]);

  if (!open) return null;

  return createPortal(
    <div
      className={`fixed inset-0 z-50 flex justify-center ${
        align === 'top' ? 'items-start pt-[10vh]' : 'items-center'
      }`}
      onMouseDown={closeOnOverlay ? (e) => {
        // mousedown 而非 click：在面板里按下、拖到遮罩上松手（选文本）不该关窗
        if (e.target === e.currentTarget) onClose();
      } : undefined}
    >
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm pointer-events-none" />
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-label={typeof title === 'string' ? title : ariaLabel}
        tabIndex={-1}
        className={`relative flex flex-col overflow-hidden bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] animate-slide-in outline-none ${panelClassName}`}
        onMouseDown={(e) => e.stopPropagation()}
      >
        {title !== undefined && (
          <div className="flex items-center justify-between gap-3 px-5 py-4 border-b border-[var(--border-subtle)] flex-shrink-0">
            <h2 className="text-lg font-semibold text-[var(--text-primary)] truncate">{title}</h2>
            <div className="flex items-center gap-2 flex-shrink-0">
              {headerExtra}
              <ModalCloseButton onClose={onClose} />
            </div>
          </div>
        )}
        {children}
      </div>
    </div>,
    document.body,
  );
}

/** 供自定义头部复用的关闭按钮（尺寸/命中区统一）。 */
export function ModalCloseButton({ onClose, label }: { onClose: () => void; label?: string }) {
  const t = useT();
  // 默认文案走 i18n：这个 label 同时喂给 title（鼠标悬停可见），
  // 写死 'Close' 会让中文界面下最常用的几个弹窗冒出一个英文 tooltip
  const text = label ?? t('prompt.close');
  return (
    <button
      type="button"
      className="w-7 h-7 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
      onClick={onClose}
      aria-label={text}
      title={text}
    >
      <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round">
        <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
      </svg>
    </button>
  );
}

import { useT } from '../i18n';
import type { PaneStatus } from '../types';

/**
 * pane / 项目状态指示。
 *
 * 四态原先只用颜色区分（6px 纯色圆点），红绿黄对色觉障碍用户几乎不可分辨，
 * ai-working 还叠了 0.8s 无限闪。现在是 **形状 + 颜色** 双编码：
 *   idle       空心细圈
 *   ai-idle    实心圆 + 对勾（已完成）
 *   ai-working 半填充圆环（进行中）
 *   error      实心圆 + 叉
 * 呼吸动画保留但由 `prefers-reduced-motion` 兜底（见 styles.css）。
 */

const STATUS_COLORS: Record<PaneStatus, string> = {
  idle: 'var(--text-muted)',
  'ai-idle': 'var(--color-success)',
  'ai-working': 'var(--color-ai-working)',
  error: 'var(--color-error)',
};

function StatusGlyph({ status }: { status: PaneStatus }) {
  switch (status) {
    case 'ai-idle':
      return (
        <>
          <circle cx="8" cy="8" r="6.5" fill="currentColor" />
          <path d="M5 8.2l2 2 4-4.2" fill="none" stroke="var(--bg-elevated)" strokeWidth="2"
            strokeLinecap="round" strokeLinejoin="round" />
        </>
      );
    case 'ai-working':
      // 底圈 + 一段亮弧 = spinner。这个形状本身就在说「正在转」，
      // 所以它必须真的转（见下面的 animate-status-spin）—— 画着一段弧却纹丝不动，
      // 看上去就是个卡死的加载指示器。
      return (
        <>
          <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeWidth="1.6" opacity="0.3" />
          <path d="M8 2a6 6 0 0 1 6 6" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />
        </>
      );
    case 'error':
      return (
        <>
          <circle cx="8" cy="8" r="6.5" fill="currentColor" />
          <path d="M5.6 5.6l4.8 4.8M10.4 5.6l-4.8 4.8" fill="none" stroke="var(--bg-elevated)"
            strokeWidth="2" strokeLinecap="round" />
        </>
      );
    default:
      return <circle cx="8" cy="8" r="4.5" fill="none" stroke="currentColor" strokeWidth="1.8" />;
  }
}

export function StatusDot({ status, size = 'sm' }: { status: PaneStatus; size?: 'sm' | 'md' }) {
  const t = useT();
  const px = size === 'sm' ? 10 : 13;
  // 文案走 i18n —— 之前这里直接把枚举名 `ai-working` 当 tooltip 给用户看
  const label = t(`panels.statusDot.${status}`);
  return (
    <svg
      width={px}
      height={px}
      viewBox="0 0 16 16"
      className={`flex-shrink-0 ${status === 'ai-working' ? 'animate-status-spin' : ''}`}
      style={{ color: STATUS_COLORS[status] }}
      role="img"
      aria-label={label}
    >
      <title>{label}</title>
      <StatusGlyph status={status} />
    </svg>
  );
}

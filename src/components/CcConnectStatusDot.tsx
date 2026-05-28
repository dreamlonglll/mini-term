import { useAppStore } from '../store';

interface Props {
  /** 点击时跳到设置页的 cc-connect 栏 */
  onOpenSettings: () => void;
}

/**
 * 顶部标题栏的 cc-connect 状态点。
 * - 绿色 ● = running(全局 5s 轮询保持新鲜)
 * - 灰色 ○ = stopped(诊断为空)
 * - 红色 ⚠ = error(端口/token/配置异常)
 * 仅在用户已在设置里配置过 ccConnect 时才渲染,避免对未启用集成的用户造成噪音。
 */
export function CcConnectStatusDot({ onOpenSettings }: Props) {
  const ccConnect = useAppStore((s) => s.config.ccConnect);
  const status = useAppStore((s) => s.ccConnectStatus);

  // 未配置过 ccConnect 时不显示;用户进入"设置 → cc-connect"配置后才会出现。
  if (!ccConnect) return null;

  const { color, glyph, label } = (() => {
    if (!status) return { color: 'var(--text-muted)', glyph: '○', label: '未知' };
    if (status.running) return { color: 'var(--color-success)', glyph: '●', label: '运行中' };
    if (status.diagnostic) return { color: 'var(--color-error)', glyph: '⚠', label: '错误' };
    return { color: 'var(--text-muted)', glyph: '○', label: '未启动' };
  })();

  const tooltipLines: string[] = [`cc-connect ${label}`];
  if (status?.running) {
    tooltipLines.push(`端口 ${status.port}`);
    if (status.ownPid) tooltipLines.push(`pid ${status.ownPid}`);
    if (status.version) tooltipLines.push(`版本 ${status.version}`);
  } else if (status?.diagnostic) {
    tooltipLines.push(status.diagnostic);
  }
  tooltipLines.push('点击打开 cc-connect 设置');

  return (
    <span
      data-no-drag
      className="flex items-center gap-1 cursor-pointer text-[10px] text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
      onClick={onOpenSettings}
      title={tooltipLines.join('\n')}
    >
      <span data-status-dot style={{ color }} className="leading-none text-xs">
        {glyph}
      </span>
      <span>cc-connect</span>
    </span>
  );
}

import { useAppStore } from '../store';

interface Props {
  /** 单击时跳到设置页的 cc-connect 栏 */
  onOpenSettings: () => void;
  /** 双击时打开 cc-connect Dashboard 弹窗(可选;未提供则双击退化为单击行为) */
  onOpenDashboard?: () => void;
}

/**
 * 顶部标题栏的 cc-connect 状态点。
 * - 绿色 ● = running(全局 5s 轮询保持新鲜)
 * - 灰色 ○ = stopped(诊断为空)
 * - 红色 ⚠ = error(端口/token/配置异常)
 *
 * 单击:打开"设置 → cc-connect"
 * 双击:打开 Dashboard 弹窗(仅 running 时生效,通过 onOpenDashboard 触发)
 *
 * 仅在用户已在设置里配置过 ccConnect 时才渲染,避免对未启用集成的用户造成噪音。
 */
export function CcConnectStatusDot({ onOpenSettings, onOpenDashboard }: Props) {
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
  tooltipLines.push('点击打开设置 · 双击打开 Dashboard');

  return (
    <span
      data-no-drag
      className="flex items-center gap-1 cursor-pointer text-[10px] text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors select-none"
      onClick={onOpenSettings}
      onDoubleClick={(e) => {
        // 双击事件依赖单击不打开 dashboard,这里阻断单击对应的 setConfigOpen
        // 浏览器原生 dblclick 之前会先触发两次 click,但本场景 onOpenSettings 仅是
        // 打开 settings modal,即使先开 settings 再开 dashboard 也能接受;
        // 用 stopPropagation 防止冒泡到 titlebar drag handler。
        e.stopPropagation();
        if (status?.running && onOpenDashboard) onOpenDashboard();
      }}
      title={tooltipLines.join('\n')}
    >
      <span data-status-dot style={{ color }} className="leading-none text-xs">
        {glyph}
      </span>
      <span>cc-connect</span>
    </span>
  );
}

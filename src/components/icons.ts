/**
 * 通用 UI 图标统一出口（lucide-react 具名再导出，天然 tree-shake）。
 *
 * 规范（与存量手绘 SVG 视觉衔接）：
 * - 尺寸:树/标签 13–14px,空态 40px;线宽 strokeWidth={1.5}(手绘为 1.4,肉眼无缝)。
 * - 颜色:一律 currentColor,由现有 CSS 变量(--accent / 语义色)控制,换主题自动跟随。
 * - 装饰性图标须加 aria-hidden,不污染读屏。
 *
 * 约定:新代码只从此出口取通用图标,不直接 import lucide-react;
 * 新增图标 = 在下面追加一行再导出。存量手绘 SVG 按触碰顺序渐进迁移。
 */
export { Bot, Boxes, Package, Server } from 'lucide-react';

/**
 * AI 品牌图标(@lobehub/icons)。
 *
 * 工程红线(违反即把 antd 生态 / 57MB 全量图标拖进主 bundle):
 * 1. 必须深路径 import(es/{Brand}/components/Color|Mono),禁止 `import { X } from '@lobehub/icons'`;
 * 2. 只用 Color / Mono 纯 SVG 组件,禁用 .Avatar 系列(依赖 @lobehub/ui + antd-style)。
 *
 * Mono 组件走 currentColor,自动跟随主题;Color 组件品牌原色呈现。
 * 未知厂商回退 lucide Bot(muted 色)。外层统一挂 .mt-icon-brand,主题皮肤可整体调滤镜。
 *
 * 商标注意:品牌 logo 仅作「该会话属于哪家 AI」的指示性使用,不得用作产品自身标识。
 */
import ClaudeColor from '@lobehub/icons/es/Claude/components/Color';
import GeminiColor from '@lobehub/icons/es/Gemini/components/Color';
import QwenColor from '@lobehub/icons/es/Qwen/components/Color';
import DeepSeekColor from '@lobehub/icons/es/DeepSeek/components/Color';
import OpenAIMono from '@lobehub/icons/es/OpenAI/components/Mono';
import GrokMono from '@lobehub/icons/es/Grok/components/Mono';
import OpenCodeMono from '@lobehub/icons/es/OpenCode/components/Mono';
import GithubCopilotMono from '@lobehub/icons/es/GithubCopilot/components/Mono';
import OllamaMono from '@lobehub/icons/es/Ollama/components/Mono';
import { Bot } from './icons';
import type { AiVendor } from '../utils/inferVendor';
import type { ComponentType } from 'react';

/**
 * pi(pi.dev,earendil-works/pi)官方标记,取自其 Press Kit 的 currentColor 版本。
 *
 * 不能用 `@lobehub/icons/es/Pi`:那是 Inflection AI 的 pi.ai,与本 agent 无从属关系,
 * 挂上去等于标错厂商。viewBox 收到图形自身的包围盒(官网 hero 版同此裁剪),
 * 否则原始 800×800 画布留白 20%,并排时比其他品牌图标明显小一圈。
 */
function PiMark({ size = 13 }: { size?: number | string }) {
  return (
    <svg
      viewBox="165.29 165.29 469.43 469.43"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden
    >
      <path
        fillRule="evenodd"
        d="M165.29 165.29H517.36V400H400V517.36H282.65V634.72H165.29ZM282.65 282.65V400H400V282.65Z"
      />
      <path d="M517.36 400H634.72V634.72H517.36Z" />
    </svg>
  );
}

const BRAND_ICONS: Record<AiVendor, ComponentType<{ size?: number | string }>> = {
  claude: ClaudeColor,
  openai: OpenAIMono,
  pi: PiMark,
  gemini: GeminiColor,
  opencode: OpenCodeMono,
  grok: GrokMono,
  qwen: QwenColor,
  deepseek: DeepSeekColor,
  copilot: GithubCopilotMono,
  ollama: OllamaMono,
};

/** 只有 Mono 变体的品牌指定代表色(官方 logo 本为黑白,借品牌主色提辨识度);
 *  未列出的 Mono 品牌(Grok/OpenCode/Ollama 官方即黑色)维持 currentColor 跟随主题。 */
const MONO_BRAND_COLORS: Partial<Record<AiVendor, string>> = {
  openai: '#10a37f',
  copilot: '#8957e5',
};

interface Props {
  vendor: AiVendor | null | undefined;
  size?: number;
  title?: string;
  className?: string;
}

export function BrandIcon({ vendor, size = 13, title, className }: Props) {
  const Icon = vendor ? BRAND_ICONS[vendor] : undefined;
  const monoColor = vendor ? MONO_BRAND_COLORS[vendor] : undefined;
  return (
    <span
      className={`mt-icon mt-icon-brand inline-flex items-center flex-shrink-0 ${className ?? ''}`}
      style={monoColor ? { color: monoColor } : undefined}
      title={title}
      aria-hidden
    >
      {Icon ? (
        <Icon size={size} />
      ) : (
        <Bot size={size} strokeWidth={1.5} className="text-[var(--text-muted)]" />
      )}
    </span>
  );
}

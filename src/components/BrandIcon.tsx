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

const BRAND_ICONS: Record<AiVendor, ComponentType<{ size?: number | string }>> = {
  claude: ClaudeColor,
  openai: OpenAIMono,
  gemini: GeminiColor,
  opencode: OpenCodeMono,
  grok: GrokMono,
  qwen: QwenColor,
  deepseek: DeepSeekColor,
  copilot: GithubCopilotMono,
  ollama: OllamaMono,
};

interface Props {
  vendor: AiVendor | null | undefined;
  size?: number;
  title?: string;
  className?: string;
}

export function BrandIcon({ vendor, size = 13, title, className }: Props) {
  const Icon = vendor ? BRAND_ICONS[vendor] : undefined;
  return (
    <span
      className={`mt-icon mt-icon-brand inline-flex items-center flex-shrink-0 ${className ?? ''}`}
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

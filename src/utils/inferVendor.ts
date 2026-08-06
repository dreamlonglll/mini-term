/**
 * 从 AI 会话线索推断厂商 key(纯函数,无依赖,可进 node --test)。
 *
 * 输入优先级:hook 上报的 agent 类型(权威)> 启动器/输入命令文本(兜底)。
 * 返回 null = 识别不出,调用方回退通用 Bot 图标。
 */

import type { AiVendor } from '../types';

export type { AiVendor };

// 顺序即优先级:openai 的关键词面最宽(gpt/o1–o4),放最后避免误伤;
// o1–o4 系列用前后非字母数字的 \b 边界防止匹配到普通单词(如 "foo3")。
const RULES: [AiVendor, RegExp][] = [
  // pi 放最前:它是多模型 harness,`pi --model claude-sonnet-5` 这种命令下
  // harness 才是该显示的厂商。反向不会误伤——claude / copilot / opencode
  // 里的 "pi" 都不在词边界上(\bpi\b 不匹配 "copilot"),pip/ping 同理。
  ['pi', /\bpi\b/i],
  ['claude', /\b(claude|anthropic)\b/i],
  ['gemini', /\bgemini\b/i],
  ['opencode', /\bopencode\b/i],
  ['grok', /\b(grok|xai)\b/i],
  ['qwen', /\b(qwen|dashscope)\b/i],
  ['deepseek', /\bdeepseek\b/i],
  ['copilot', /\bcopilot\b/i],
  ['ollama', /\bollama\b/i],
  ['openai', /\b(codex|openai|gpt|o[1-4])\b/i],
];

export function inferVendor(input: { agent?: string; command?: string }): AiVendor | null {
  for (const source of [input.agent, input.command]) {
    if (!source) continue;
    for (const [vendor, re] of RULES) {
      if (re.test(source)) return vendor;
    }
  }
  return null;
}

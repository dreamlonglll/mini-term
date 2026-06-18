/**
 * 终端 Unicode 宽度修正：在官方 @xterm/addon-unicode11 之上修正 VS16(emoji
 * presentation)宽度，让 box-drawing 表格里的 emoji 列对齐。
 *
 * 背景:
 * - xterm.js 默认走 UnicodeV6 宽度表,把 ✅❌⚠️ 等 emoji 判为窄字符(width 1),
 *   而生成表格的 CLI/AI 工具按 Unicode 9+ 约定(emoji = 2 格,与 Windows
 *   Terminal/iTerm2 一致)对齐填充 → 竖线列错位。
 * - 加载 addon-unicode11 后,✅(U+2705)/❌(U+274C)在其 BMP_WIDE 表里 → 修正为 2 格。
 * - 但 ⚠️ = U+26A0(WARNING SIGN) + U+FE0F(VS16):U+26A0 不在 v11 宽表里(表里只有
 *   U+26A1 ⚡),VS16 是 0 宽组合符;UnicodeV11.charProperties 遇 VS16 只保持前一个
 *   base 的宽度(1),不做 emoji presentation 提升 → ⚠️ 仍是 width 1,继续错位。
 *
 * 做法:
 * - 仍用官方 addon 注册标准 '11' 表(零转写风险,复用其已验证的宽度数据);
 * - 借其 UnicodeV11 实例包一层,注册版本 '11-emoji':遇到 VS16 紧跟一个有宽度的
 *   base 时,把整体强制提升为 width 2 并 join,与现代终端及源程序口径一致。
 * - 拿不到内部 provider(未来 xterm 改私有字段)时优雅回退到标准 '11',至少保住 ✅/❌。
 */

import type { Terminal } from '@xterm/xterm';
import { Unicode11Addon } from '@xterm/addon-unicode11';

/** VARIATION SELECTOR-16:把前一个 emoji-base 转为彩色 emoji 形态,渲染占 2 格 */
const VS16 = 0xfe0f;

/**
 * 复刻 xterm core UnicodeService 的属性打包格式(category 固定 0):
 *   bit0 = shouldJoin, bits1-2 = width, bits3+ = charKind
 * 见 node_modules/@xterm/xterm 内 UnicodeService.createPropertyValue。
 */
function packPropertyValue(width: number, shouldJoin: boolean): number {
  return ((width & 3) << 1) | (shouldJoin ? 1 : 0);
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyProvider = { version: string; wcwidth: (cp: number) => number; charProperties: (cp: number, preceding: number) => number };

/** 从 core 内部取出 addon 刚注册的 UnicodeV11 实例(复用其宽度表) */
function getBaseV11Provider(term: Terminal): AnyProvider | undefined {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const core = (term as any)._core;
  const svc = core?.unicodeService ?? core?._unicodeService;
  const base = svc?._providers?.['11'];
  if (base && typeof base.wcwidth === 'function' && typeof base.charProperties === 'function') {
    return base as AnyProvider;
  }
  return undefined;
}

/**
 * 加载 unicode11 addon 并激活带 VS16 修正的宽度表。在 new Terminal() 之后、
 * 写入任何内容之前调用。
 */
export function activateUnicodeWidth(term: Terminal): void {
  term.loadAddon(new Unicode11Addon());

  const base = getBaseV11Provider(term);
  if (!base) {
    // 内部结构取不到 → 回退标准 v11(✅/❌ 仍修正,仅 ⚠️ 这类 VS16 序列保持原状)
    term.unicode.activeVersion = '11';
    return;
  }

  term.unicode.register({
    version: '11-emoji',
    wcwidth: (cp: number) => base.wcwidth(cp) as 0 | 1 | 2,
    charProperties: (cp: number, preceding: number): number => {
      // VS16 紧跟一个有宽度的 base → emoji presentation,整体占 2 格
      if (cp === VS16 && preceding !== 0) {
        return packPropertyValue(2, true);
      }
      return base.charProperties(cp, preceding);
    },
  });
  term.unicode.activeVersion = '11-emoji';
}

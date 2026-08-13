import type { AiSessionRef, PaneState } from '../types';

/**
 * AI 会话 resume 命令的唯一拼装点。
 *
 * sessionId 会被原样拼进写入 PTY 的命令行,必须过白名单:字母数字与 -_
 * (Claude UUID、Codex rollout id 与 Grok UUIDv7 的实际形态);长度上限挡异常来源。
 * id 的两个来源——持久化布局(config.json 的 savedLayout)与会话记录文件
 * 内容(/payload/id 任意字符串)——都不是可信输入,空格/引号/管道/
 * 换行等一切 shell 元字符在此拦截,拦不住的只剩「多跑一条无害的
 * resume <乱码>」。
 *
 * grok 的 `--resume` 与 claude 同理:会话按 cwd 分桶,只有在原目录里才认得出
 * 该 id,由调用方保证 pane 的 cwd 正确。
 */
export function buildResumeCommand(agent: string | undefined, sessionId: string): string | null {
  if (!/^[A-Za-z0-9_-]{1,128}$/.test(sessionId)) return null;
  if (agent === 'codex') return `codex resume ${sessionId}`;
  if (agent === 'grok') return `grok --resume ${sessionId}`;
  return `claude --resume ${sessionId}`;
}

/**
 * 启动恢复某个 pane 时该不该自动续接、续接命令是什么。
 *
 * 汇总全部否决条件,返回 null = 不写命令:
 * - `enabled`:系统设置里的「启动自动续接 AI 会话」开关(config.aiAutoResume,
 *   缺省开启)。关掉只影响写不写命令,aiSession 身份照旧随布局持久化,
 *   重新打开后下次启动仍能续上。
 * - `resumePending`:布局恢复置位、写一次即清,防重复写。
 * - `remote`:远程 pane 的 PTY 是 ssh 启动器,启动初期可能停在口令交互上,
 *   预写的命令会被当口令消费;远端会话身份也不来自本机 hook。
 * - id 非法:见 buildResumeCommand 的白名单。
 *
 * 注意 `enabled=false` 时调用方**不该**清 resumePending:标记的语义是「这个
 * pane 还没续过」,不是「这次启动没续」。开关中途打开后,点开那些尚未起 PTY
 * 的 pane 仍应续上,清了就续不了了。
 */
export function resolveAutoResumeCommand(opts: {
  enabled: boolean;
  resumePending: boolean | undefined;
  session: AiSessionRef | undefined;
  remote: boolean;
}): string | null {
  if (!opts.enabled || !opts.resumePending || !opts.session || opts.remote) return null;
  return buildResumeCommand(opts.session.agent, opts.session.sessionId);
}

/**
 * pane 该不该按「AI 会话」展示 —— pane 标签页的品牌图标、项目列表的 AI 图标堆叠
 * 共用这一条判定(两处曾各写各的,加了续接开关后就漂了)。
 *
 * 亮图标的两种情形:
 * - 正在跑(status 为 ai-*):hook 或输入检测已经确认,与开关无关。
 * - 持有会话身份且「即将续接」:恢复出来还没起 PTY 的 pane 不该等切过去才出图标。
 *   但 resumePending 且开关关着 = 这个 pane 起来就是个普通 shell,不能亮 —— 身份
 *   还留着只是为了开关重开后仍能续上,不代表里面跑着 AI。
 */
export function paneShowsAiSession(
  pane: Pick<PaneState, 'status' | 'aiSession' | 'resumePending'>,
  autoResumeEnabled: boolean,
): boolean {
  if (pane.status === 'ai-working' || pane.status === 'ai-idle') return true;
  if (!pane.aiSession) return false;
  return !pane.resumePending || autoResumeEnabled;
}

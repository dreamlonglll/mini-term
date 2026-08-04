/**
 * AI 会话 resume 命令的唯一拼装点。
 *
 * sessionId 会被原样拼进写入 PTY 的命令行,必须过白名单:字母数字与 -_
 * (Claude UUID 与 Codex rollout id 的实际形态);长度上限挡异常来源。
 * id 的两个来源——持久化布局(config.json 的 savedLayout)与会话 JSONL
 * 文件内容(/payload/id 任意字符串)——都不是可信输入,空格/引号/管道/
 * 换行等一切 shell 元字符在此拦截,拦不住的只剩「多跑一条无害的
 * resume <乱码>」。
 */
export function buildResumeCommand(agent: string | undefined, sessionId: string): string | null {
  if (!/^[A-Za-z0-9_-]{1,128}$/.test(sessionId)) return null;
  return agent === 'codex' ? `codex resume ${sessionId}` : `claude --resume ${sessionId}`;
}

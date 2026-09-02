/**
 * 编排体系里**会被写进别人终端**的那些文案（ADR 0003 / 0004）。
 *
 * 与其它命名空间不同：这里的字符串不进界面，而是随派活的 prompt 或汇报
 * 一起写穿进 AI 会话的终端，读者是另一个 LLM。所以措辞按「指令」写，
 * 不按「界面提示」写；术语一律「受编排会话 / orchestrated session」，
 * 不出现内部口语别名（乐手 / musician）。
 */
export const orchestrator = {
  zh: {
    reportFooter:
      "【来自编排者的固定要求】做完后请按这个格式收尾：结果 / 改动的文件 / 做过的验证 / 未完成或存疑的事。遇到拿不准的问题，用文字写出来并结束本回合等答复，不要使用交互式提问工具。",
  },
  en: {
    reportFooter:
      "[Standing instruction from your orchestrator] When you are done, close with this format: result / files changed / checks you ran / anything unfinished or uncertain. If something is unclear, write it out as plain text and end the turn to wait for an answer - do not use an interactive question tool.",
  },
} as const;

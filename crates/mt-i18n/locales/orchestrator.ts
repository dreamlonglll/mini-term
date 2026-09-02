/**
 * 编排体系里**会被另一个 LLM 读到**的那些文案（ADR 0003 / 0004）。
 *
 * 与其它命名空间不同：这里的字符串不进界面。`reportFooter` 随派活的 prompt
 * 写穿进受编排会话的终端；其余几条是**汇报文件的正文**（编排者用自己的 Read
 * 工具读）。读者都是另一个 LLM，所以措辞按「指令」写，不按「界面提示」写；
 * 术语一律「受编排会话 / orchestrated session」，不出现内部口语别名
 * （乐手 / musician）。
 *
 * ⚠️ **不许出现 ESC 序列，也别写裸换行**：`reportFooter` 会被包进 bracketed
 * paste 送进终端（自带 `ESC[201~` 就能把粘贴块提前截断），而汇报文件里的每一
 * 条都占**一整行**——自带换行会把那一行劈开。
 *
 * ⚠️ 汇报文件的**抬头字段名与 `kind` 的取值不在这里**：它们是稳定的 ASCII
 * （`session` / `kind: turn-ended` …），编排者要拿它们做分支判断，翻译它们
 * 就等于让它在两种显示语言下认两套字段名。见 `mt_ai::control::report_header`。
 */
export const orchestrator = {
  zh: {
    reportFooter:
      "【来自编排者的固定要求】做完后请按这个格式收尾：结果 / 改动的文件 / 做过的验证 / 未完成或存疑的事。遇到拿不准的问题，用文字写出来并结束本回合等答复，不要使用交互式提问工具。",

    // ── 汇报文件的正文 ──
    awaitingHumanNote:
      "你不能替人回答。把下面的画面原文转述给用户，请人到那个终端处理完，再继续派活。",
    notAcceptedNote:
      "写入后 15 秒仍没看到它开始处理。用 read-screen 核对那个终端此刻的样子，必要时重发。",
    transcriptHeader: "会话记录增量（第 {from} 条起，共 {count} 条）：",
    transcriptEmpty:
      "（这一段还没有新的会话记录，需要的话稍后用 read-transcript 补读）",
    bodyTruncated:
      "（内容过长，这里截断了；完整内容用 read-transcript 或 read-screen 自己取）",
    screenHeader: "终端画面尾部原文：",
    screenUnavailable: "（读不到这个终端此刻的画面）",
  },
  en: {
    reportFooter:
      "[Standing instruction from your orchestrator] When you are done, close with this format: result / files changed / checks you ran / anything unfinished or uncertain. If something is unclear, write it out as plain text and end the turn to wait for an answer - do not use an interactive question tool.",

    awaitingHumanNote:
      "You must not answer on the user's behalf. Relay the terminal text below to the user, let a human deal with it in that terminal, and only then send more work.",
    notAcceptedNote:
      "15 seconds after the write it still had not started working on it. Use read-screen to check what that terminal looks like now, and resend if needed.",

    transcriptHeader: "New transcript entries (from #{from}, {count} in total):",
    transcriptEmpty:
      "(no new transcript entries for this stretch yet - use read-transcript later if you need them)",
    bodyTruncated:
      "(too long, truncated here; fetch the rest yourself with read-transcript or read-screen)",
    screenHeader: "Tail of the terminal screen:",
    screenUnavailable: "(cannot read that terminal's screen right now)",
  },
} as const;

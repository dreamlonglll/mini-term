/**
 * 编排体系里**会被写进别人终端**的那些文案（ADR 0003 / 0004）。
 *
 * 与其它命名空间不同：这里的字符串不进界面，而是随派活的 prompt 或汇报
 * 一起写穿进 AI 会话的终端，读者是另一个 LLM。所以措辞按「指令」写，
 * 不按「界面提示」写；术语一律「受编排会话 / orchestrated session」，
 * 不出现内部口语别名（乐手 / musician）。
 *
 * ⚠️ 这里的每一条都会被包进 bracketed paste 送进终端：**不许出现 ESC 序列**
 * （自带 `ESC[201~` 就能把粘贴块提前截断），也别写裸换行——装配那一侧会把
 * 换行统统归一成 `\r`。
 */
export const orchestrator = {
  zh: {
    reportFooter:
      "【来自编排者的固定要求】做完后请按这个格式收尾：结果 / 改动的文件 / 做过的验证 / 未完成或存疑的事。遇到拿不准的问题，用文字写出来并结束本回合等答复，不要使用交互式提问工具。",

    // ── 汇报的批头（工单 12）──
    batchHeader:
      "以下是 mini-term 桌面端自动送达的受编排会话汇报，共 {count} 条。用户没有说话——是你派出去的受编排会话有了新进展。请据此决定下一步；需要人处理的事直接告诉用户。",
    batchDropped:
      "另有 {dropped} 条汇报因积压被丢弃，需要完整经过请自己用 read-transcript 补读。",

    // ── 每条汇报的抬头字段（按 · 依次拼接）──
    sessionField: "受编排会话 #{pane}",
    launcherField: "启动器 {launcher}",
    projectField: "项目 {project}",
    causeField: "成因 {cause}",
    durationField: "本回合用时 {duration}",
    tasksField: "涉及任务 {tasks}",

    // ── 五种汇报各自那句话 ──
    turnEnded: "回合结束",
    awaitingHuman: "停下等人处理",
    awaitingHumanNote:
      "你不能替人回答。把下面的画面原文转述给用户，请人到那个终端处理完，再继续派活。",
    exited: "里头的 AI 已退出，终端退回普通 shell",
    closed: "终端已被关闭，这个受编排会话不会再有后续",
    notAccepted: "任务 {task} 可能没被接收",
    notAcceptedNote:
      "写入后 15 秒仍没看到它开始处理。用 read-screen 核对那个终端此刻的样子，必要时重发。",

    // ── 正文 ──
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

    batchHeader:
      "The following are automatic reports from the mini-term desktop about your orchestrated sessions ({count} in total). The user did not say this - one of the sessions you dispatched has made progress. Decide the next step from it; anything a human must handle, tell the user.",
    batchDropped:
      "{dropped} further report(s) were dropped because they piled up; use read-transcript if you need the full story.",

    sessionField: "orchestrated session #{pane}",
    launcherField: "launcher {launcher}",
    projectField: "project {project}",
    causeField: "cause {cause}",
    durationField: "turn took {duration}",
    tasksField: "tasks involved: {tasks}",

    turnEnded: "turn ended",
    awaitingHuman: "stopped, waiting for a human",
    awaitingHumanNote:
      "You must not answer on the user's behalf. Relay the terminal text below to the user, let a human deal with it in that terminal, and only then send more work.",
    exited: "the AI inside it exited; the terminal is back to a plain shell",
    closed:
      "that terminal has been closed; this orchestrated session will produce nothing more",
    notAccepted: "task {task} may not have been picked up",
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

export const sessions = {
  zh: {
    title: "活跃 AI 会话",
    empty: "当前没有活跃的 AI 会话",
    emptyHint: "在桌面端 mini-term 的终端里启动 Claude / Codex 后，对应 pane 会实时出现在这里。",
    offlineBanner: "桌面端离线",
    offlineHint: "mini-term 未连接中转服务器，列表暂不可用；桌面端恢复后自动更新。",
    status: {
      aiWorking: "工作中",
      aiIdle: "空闲",
      error: "错误",
    },
  },
  en: {
    title: "Active AI Sessions",
    empty: "No active AI sessions",
    emptyHint: "Start Claude / Codex in a desktop mini-term terminal and the pane will appear here in real time.",
    offlineBanner: "Desktop offline",
    offlineHint: "mini-term is not connected to the relay; the list is unavailable until the desktop comes back.",
    status: {
      aiWorking: "Working",
      aiIdle: "Idle",
      error: "Error",
    },
  },
} as const;

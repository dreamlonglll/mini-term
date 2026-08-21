export const markerList = {
  zh: {
    empty: "暂无标记",
    inProgress: "正在进行",
    pendingAnchor: "这条还在 AI 的队列里，等它被处理后才能跳转到对应位置",
  },
  en: {
    empty: "No markers",
    inProgress: "In progress",
    pendingAnchor:
      "Still queued in the AI session — jumping to it becomes available once the AI gets to it",
  },
} as const;

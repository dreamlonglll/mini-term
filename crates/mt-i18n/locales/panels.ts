/**
 * 常驻面板的标题与通用标签。
 *
 * 这些串原先是硬编码英文（`Projects` / `Sessions` / `Files —` / `History` /
 * `Staged Changes` …），中文界面下夹着一片没翻译的英文，是 i18n 最显眼的漏网处。
 */
export const panels = {
  zh: {
    projects: "项目",
    sessions: "会话",
    files: "文件",
    filesOf: "文件 — {project}",
    git: "Git",
    history: "提交历史",
    changes: "更改",
    stagedChanges: "已暂存",
    unstagedChanges: "未暂存",
    untrackedFiles: "未跟踪",
    commitPlaceholder: "提交说明…",
    commit: "提交 ({count})",
    stage: "暂存此文件",
    unstage: "取消暂存",
    done: "完成",
    statusDot: {
      idle: "空闲",
      "ai-idle": "AI 已完成",
      "ai-working": "AI 运行中",
      error: "出错",
    },
  },
  en: {
    projects: "Projects",
    sessions: "Sessions",
    files: "Files",
    filesOf: "Files — {project}",
    git: "Git",
    history: "History",
    changes: "Changes",
    stagedChanges: "Staged Changes",
    unstagedChanges: "Changes",
    untrackedFiles: "Untracked Files",
    commitPlaceholder: "Commit message…",
    commit: "Commit ({count})",
    stage: "Stage this file",
    unstage: "Unstage",
    done: "DONE",
    statusDot: {
      idle: "Idle",
      "ai-idle": "AI done",
      "ai-working": "AI working",
      error: "Error",
    },
  },
} as const;

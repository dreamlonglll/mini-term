export const commitDiff = {
  zh: {
    fileCount: "{count} 个文件变更",
    sideBySide: "并排",
    inline: "内联",
    loading: "加载中...",
    binaryFile: "二进制文件，不支持 diff 预览",
    tooLarge: "文件过大（>1MB），不支持 diff 预览",
    noChanges: "该提交无文件变更",
  },
  en: {
    fileCount: "{count} files changed",
    sideBySide: "Side by side",
    inline: "Inline",
    loading: "Loading...",
    binaryFile: "Binary file, diff preview not supported",
    tooLarge: "File too large (>1MB), diff preview not supported",
    noChanges: "No file changes in this commit",
  },
} as const;

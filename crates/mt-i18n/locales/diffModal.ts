export const diffModal = {
  zh: {
    sideBySide: "并排",
    inline: "内联",
    prevChange: "上一处改动",
    nextChange: "下一处改动",
    loading: "加载中...",
    binaryNotSupported: "二进制文件，不支持 diff 预览",
    tooLarge: "文件过大（>1MB），不支持 diff 预览",
  },
  en: {
    sideBySide: "Side by side",
    inline: "Inline",
    prevChange: "Previous change",
    nextChange: "Next change",
    loading: "Loading...",
    binaryNotSupported: "Binary file, diff preview not supported",
    tooLarge: "File too large (>1MB), diff preview not supported",
  },
} as const;

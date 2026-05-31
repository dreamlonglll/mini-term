export const app = {
  zh: {
    menu: {
      settings: "设置",
      connect: "连接",
    },
    update: {
      badge: "新版本 {version}",
      title: "新版本 {version} 可用，点击前往下载",
    },
    closeConfirm: {
      title: "关闭确认",
      message: "确定要关闭 Mini-Term 吗？",
    },
    emptyState: "请先在左栏添加项目",
    wslOverride: "已检测到 WSL 项目,使用 wsl.exe 启动终端 ({path})",
  },
  en: {
    menu: {
      settings: "Settings",
      connect: "Connect",
    },
    update: {
      badge: "New version {version}",
      title: "New version {version} available, click to download",
    },
    closeConfirm: {
      title: "Confirm Close",
      message: "Are you sure you want to close Mini-Term?",
    },
    emptyState: "Add a project in the left panel first",
    wslOverride: "WSL project detected, launching terminal with wsl.exe ({path})",
  },
} as const;

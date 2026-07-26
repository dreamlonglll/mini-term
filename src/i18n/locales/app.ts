export const app = {
  zh: {
    menu: {
      settings: "设置",
    },
    activityBar: {
      collapse: "折叠中间栏",
      expand: "展开中间栏",
      sessions: "会话",
      git: "Git 变更",
      settings: "设置",
      ssh: "SSH 连接",
      mobile: "移动端",
      closeDrawer: "关闭",
    },
    update: {
      badge: "新版本 {version}",
      title: "新版本 {version} 可用，点击前往下载",
    },
    closeConfirm: {
      title: "关闭确认",
      titleAi: "有 AI 会话正在运行",
      message: "还有 {count} 个终端在运行，关闭后它们会被终止。确定退出吗？",
      messageWithSessions: "还有 {count} 个终端在运行，其中这些 AI 会话会被终止：\n\n{names}\n\n确定退出吗？",
    },
    emptyState: "请先在中间栏添加项目",
    firstRun: {
      title: "还没有项目",
      subtitle: "添加一个目录，就能在里面开终端、跑 AI 会话",
      addLocal: "添加本地项目",
      addRemote: "添加 SSH 远程项目",
      hintsTitle: "常用快捷键",
    },
    wslOverride: "已检测到 WSL 项目,使用 wsl.exe 启动终端 ({path})",
    mobileStartSession: "移动端新建了会话（{launcher}）",
  },
  en: {
    menu: {
      settings: "Settings",
    },
    activityBar: {
      collapse: "Collapse panel",
      expand: "Expand panel",
      sessions: "Sessions",
      git: "Git changes",
      settings: "Settings",
      ssh: "SSH connections",
      mobile: "Mobile",
      closeDrawer: "Close",
    },
    update: {
      badge: "New version {version}",
      title: "New version {version} available, click to download",
    },
    closeConfirm: {
      title: "Confirm Close",
      titleAi: "AI sessions still running",
      message: "{count} terminal(s) are still running and will be terminated. Quit anyway?",
      messageWithSessions: "{count} terminal(s) are still running. These AI sessions will be terminated:\n\n{names}\n\nQuit anyway?",
    },
    emptyState: "Add a project in the middle panel first",
    firstRun: {
      title: "No projects yet",
      subtitle: "Add a directory to open terminals and run AI sessions in it",
      addLocal: "Add local project",
      addRemote: "Add SSH remote project",
      hintsTitle: "Handy shortcuts",
    },
    wslOverride: "WSL project detected, launching terminal with wsl.exe ({path})",
    mobileStartSession: "Mobile started a session ({launcher})",
  },
} as const;

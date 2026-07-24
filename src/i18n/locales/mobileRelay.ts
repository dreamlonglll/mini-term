export const mobileRelay = {
  zh: {
    intro:
      "移动端体系：桌面端与自托管中转服务器保持长连，手机经中转查看活跃 AI 会话并发送指令。填写你的中转服务器地址后连接自动建立。",
    urlLabel: "中转服务器地址",
    urlPlaceholder: "wss://relay.example.com",
    apply: "保存并连接",
    clear: "断开并清除",
    statusLabel: "连接状态",
    status: {
      disconnected: "未连接",
      connecting: "连接中…",
      connected: "已连接",
      reconnecting: "重连中…",
      versionMismatch: "协议版本不匹配（桌面端 v{actual}，中转要求 v{expected}），请升级 mini-term 或中转服务器",
    },
  },
  en: {
    intro:
      "Mobile client system: the desktop keeps a persistent connection to your self-hosted relay server; your phone connects through the relay to watch active AI sessions and send commands. Fill in your relay address to establish the connection.",
    urlLabel: "Relay server address",
    urlPlaceholder: "wss://relay.example.com",
    apply: "Save & connect",
    clear: "Disconnect & clear",
    statusLabel: "Connection status",
    status: {
      disconnected: "Disconnected",
      connecting: "Connecting…",
      connected: "Connected",
      reconnecting: "Reconnecting…",
      versionMismatch: "Protocol version mismatch (desktop v{actual}, relay expects v{expected}) — please upgrade mini-term or the relay server",
    },
  },
} as const;

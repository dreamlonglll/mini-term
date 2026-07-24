export const pair = {
  zh: {
    appName: "mini-term 移动端",
    scanHint: "请在桌面端 mini-term 的「移动端」面板生成二维码，然后用本机扫码打开。",
    pairing: "配对中…",
    connecting: "连接中…",
    reconnecting: "重连中…",
    revoked: "配对已失效，请重新扫码",
    revokedHint: "新设备完成了配对，或桌面端重置了配对。请在桌面端重新生成二维码。",
    rejected: {
      invalidPairingCode: "配对码无效或已过期，请在桌面端重新生成二维码",
      invalidCredential: "凭证已失效，请重新扫码配对",
      versionMismatch: "协议版本不匹配，请升级 mini-term 与中转服务器",
      missingAuth: "缺少配对信息，请通过桌面端二维码打开本页面",
    },
    language: "语言",
  },
  en: {
    appName: "mini-term Mobile",
    scanHint: "Generate a QR code in the desktop mini-term \"Mobile\" panel, then scan it with this device.",
    pairing: "Pairing…",
    connecting: "Connecting…",
    reconnecting: "Reconnecting…",
    revoked: "Pairing revoked — please scan again",
    revokedHint: "Another device completed pairing, or the desktop reset pairing. Generate a new QR code on the desktop.",
    rejected: {
      invalidPairingCode: "Pairing code invalid or expired — regenerate the QR code on the desktop",
      invalidCredential: "Credential revoked — please scan to pair again",
      versionMismatch: "Protocol version mismatch — please upgrade mini-term and the relay server",
      missingAuth: "No pairing info — open this page via the desktop QR code",
    },
    language: "Language",
  },
} as const;

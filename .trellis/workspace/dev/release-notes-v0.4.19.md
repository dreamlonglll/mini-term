## 更新内容

### 修复

- **v0.4.18 终端连体字 (ligatures) 开关完全不生效的 hotfix** — 上一版「启用连体字」开关切到 ON 后,终端无任何视觉变化,`==` `=>` `!=` `->` 仍渲染为单字符,看似功能未上线。根因:`new Terminal({...})` 配置漏开 `allowProposedApi: true`,而 `LigaturesAddon` 内部调用的 `registerCharacterJoiner` 在 xterm.js v6 中仍标记为 **proposed API**(未来可能调整签名),默认禁用。Addon 加载时抛 `"You must set the allowProposedApi option to true to use proposed API"`,错误被 `loadLigaturesIfEnabled` 内的 `try/catch` 静默吞掉(只 `console.error`),UI 表现为开关切到 ON 但终端无任何视觉变化 —— devtools 没打开根本看不出来。本版在 `new Terminal()` 显式开启该选项,Windows + Fira Code / JetBrains Mono 等含 calt 表的字体下立即生效。Spec 文档同步新增「约束 3」+ Wrong vs Correct 代码对照,标注 "错误被 try/catch 静默吞掉" 这个隐性陷阱。

## 下载

- **Windows** — `Mini-Term_0.4.19_x64-setup.exe`(NSIS,推荐)或 `Mini-Term_0.4.19_x64_en-US.msi`
- **macOS (Apple Silicon)** — `Mini-Term_0.4.19_aarch64.dmg`(首次启动需在终端运行 `xattr -cr /Applications/Mini-Term.app` 解除 Gatekeeper quarantine)
- **Linux** — `Mini-Term_0.4.19_amd64.AppImage` / `Mini-Term_0.4.19_amd64.deb` / `Mini-Term-0.4.19-1.x86_64.rpm`

> 平台支持:Windows 为主要支持平台,日常开发与测试均在 Windows 上进行;macOS / Linux 代码层面已支持,但未经充分打磨,可用性欠佳,欢迎提 Issue 反馈。

---

**完整 commits**: https://github.com/dreamlonglll/mini-term/compare/v0.4.18...v0.4.19

## 更新内容

### 新增

- **cc-connect 集成 — 把本机 AI agent 通过 IM 平台（飞书 / Slack / Telegram / Discord / 钉钉 / 微信等）远程驱动** — 与上游 [chenhg5/cc-connect](https://github.com/chenhg5/cc-connect) 桥接，入口统一收在顶栏「连接」按钮弹窗内：
  - **进程管理（零配置）** — 一键启动 / 停止 / 重启 / 测试连接 / 编辑 config.toml，可选 mini-term 启动时自动 spawn；可执行文件 / 配置路径留空时自动回退 PATH 中的 `cc-connect` 与 `~/.cc-connect/config.toml`，零配置即可使用；关闭 mini-term 不联动 kill，保证 IM 持续可用。
  - **一键导入项目** — 弹窗内列出全部 mini-term 项目，勾选 / 全选后一键批量导入到 cc-connect（用 `toml_edit` 追加 `[[projects]]` 保留注释、单次写盘 + 仅重启一次、同名冲突加 8 字符 hash 后缀），也可逐项单独导入；已导入项显示「● 已导入」并可一键移除。每个导入项目会附带一个占位 Telegram 平台保证 cc-connect 能冷启动，导入后到 Dashboard 把占位替换为真实 IM 平台即可启用。
  - **Dashboard 嵌入** — 弹窗内「打开 Dashboard」直接在 mini-term 内嵌 cc-connect Web 控制台（自动登录态），配置 IM 平台 / 切换 provider / 管理 cron，无需另开浏览器。

### 修复

- **cc-connect 在 Windows 上的进程管理兼容性** — cc-connect 常以 npm 脚本壳（`cc-connect.cmd` / `.ps1`）安装、无原生 `.exe`，旧逻辑 `Command::new` 解析裸名只补 `.exe` 导致 program not found；现按 PATH × PATHEXT 像终端一样解析（`.cmd` / `.bat` 原生 spawn、`.ps1` 包 `powershell -NoProfile -File`）。停止 / 重启遗留孤儿进程（真实 node 是脚本壳子孙、`child.kill()` 杀不到）改用 `taskkill /T /F` 杀整棵进程树。探活 HTTP 改异步执行，不再因 5s 轮询冻结 UI。
- **导入项目缺平台导致 cc-connect 无法冷启动** — cc-connect 强制每个 `[[projects]]` 至少一个 `[[projects.platforms]]`，否则配置校验失败、进程 `os.Exit(1)`。导入时为每个项目注入一个冷启动安全的占位 Telegram 平台（非空假 token，异步重连失败不崩），彻底避免写出无法启动的配置。

## 下载

- **Windows** — `Mini-Term_0.4.21_x64-setup.exe`（NSIS，推荐）或 `Mini-Term_0.4.21_x64_en-US.msi`
- **macOS (Apple Silicon)** — `Mini-Term_0.4.21_aarch64.dmg`（首次启动需在终端运行 `xattr -cr /Applications/Mini-Term.app` 解除 Gatekeeper quarantine）
- **Linux** — `Mini-Term_0.4.21_amd64.AppImage` / `Mini-Term_0.4.21_amd64.deb` / `Mini-Term-0.4.21-1.x86_64.rpm`

> 平台支持：Windows 为主要支持平台，日常开发与测试均在 Windows 上进行；macOS / Linux 代码层面已支持，但未经充分打磨，可用性欠佳，欢迎提 Issue 反馈。

---

**完整 commits**: https://github.com/dreamlonglll/mini-term/compare/v0.4.20...v0.4.21

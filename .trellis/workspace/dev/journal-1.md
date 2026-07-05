# Journal - dev (Part 1)

> AI development session journal
> Started: 2026-05-01

---



## Session 1: 修复退出 AI agent 后状态卡在 ai-idle

**Date**: 2026-05-08
**Task**: 修复退出 AI agent 后状态卡在 ai-idle
**Branch**: `main`

### Summary

SessionEnd 事件清除 hook 状态回退 idle；process_monitor 增加 ai-idle 时 AI 进程存活校验

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `41f2f86` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 2: 实现智能终端复制粘贴快捷键（issue #31）

**Date**: 2026-05-17
**Task**: 实现智能终端复制粘贴快捷键（issue #31）
**Branch**: `main`

### Summary

为 issue #31 实现智能 Ctrl+C/V 复制粘贴：新增 smartCopyPaste 配置（默认关闭），开启后 Ctrl+C 有选区复制、无选区透传 SIGINT，Ctrl+V 直接粘贴，Ctrl+Shift+C/V 保留；含终端设置页 toggle 与快捷键说明页动态化。trellis-check 通过，spec 记录 AppConfig 字段扩展契约，已回复 GitHub issue。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `2d192f5` | (see git log) |
| `a98255d` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 3: SSH 管理器 v2：私钥权限自动处理

**Date**: 2026-05-18
**Task**: SSH 管理器 v2：私钥权限自动处理
**Branch**: `main`

### Summary

完成 SSH 管理器 v2 阶段。新增后端 src-tauri/src/ssh.rs：prepare_ssh_key 命令连接前把私钥复制到权限收紧的临时副本（按源路径 DefaultHasher 稳定命名、重连复用），Windows 用 icacls /inheritance:r /grant:r 收紧 ACL、Unix 设 0600，绕过 Windows OpenSSH UNPROTECTED PRIVATE KEY FILE 拒绝；cleanup_ssh_temp_keys 启动时清理临时密钥目录。lib.rs 注册命令并接入启动清理。前端 TerminalInstance.tsx 的 connectSsh 连接前调用 prepare_ssh_key 取临时副本路径，失败 console.error 回退原始路径；buildSshCommand 签名改为 (conn, identityPath)。流程：trellis-implement 实现 → trellis-check 审查无问题 → cargo test 87 通过、npm run build 通过 → 提交 30b2182。3.3 判定无需更新 spec（复用 clipboard.rs 既有模式）。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `30b2182` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 4: 重构 mt-ssh-mcp 为 russh 持久会话池

**Date**: 2026-05-22
**Task**: 重构 mt-ssh-mcp 为 russh 持久会话池
**Branch**: `refactor/ssh-mcp-session-pool`

### Summary

把 mt-ssh-mcp sidecar 的 每次 spawn ssh 子进程 模型重构为基于 russh 0.61 的进程内 SSH 会话池：第一次调用建 session、后续 ssh_exec 复用同一 session 开 exec channel。三个 PR 切分：PR1 引入 russh + 池骨架(SshPool/MtClient Handler/known_hosts accept-new/LRU)、PR2 把 ssh_exec 切到走池并删除旧的 PTY autofill 路径、PR3 加后台 reaper(10min idle/2h lifetime)与 shutdown 钩子。中间一个 gatetime cooldown bug 修复 + 一个 dead_code 清理。沉淀 3 个 backend spec：Windows MSVC NASM 坑、rand_core 多版本坑、tokio 资源池骨架。50 sidecar 测试 + 29 mt-core 测试全过，dev/release/clippy 全 0 warning。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `7c460b0` | (see git log) |
| `5db2dad` | (see git log) |
| `ea52f9f` | (see git log) |
| `c302b99` | (see git log) |
| `0875fa2` | (see git log) |
| `d641fd6` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 5: 项目级环境变量功能

**Date**: 2026-05-26
**Task**: 项目级环境变量功能
**Branch**: `main`

### Summary

为每个项目支持自定义环境变量,新建终端 PTY 时按项目独立注入到子进程。完整 brainstorm→grill(9轮)→implement→trellis-check→update-spec→finish 流程。后端 ProjectConfig.envVars + create_pty envs 参数 + MINITERM_* 前后端双重保护 + WSL 分支跳过注入;前端独立 modal 仿 SshAssocModal、行级 enabled checkbox、inline POSIX 校验红框、保存按钮 disabled、WSL 警告条、Esc 关闭遮罩不响应、保存失败 setConfig 回滚。trellis-check 修复 3 个问题(2 阻塞:前端 isWslPath 漏 verbatim、Rust 缺 MINITERM_ 二次保护;1 建议:save_config 失败无回滚无 toast)。新增 spec backend/pty-env-vars-injection.md 完整 7 sections,frontend/state-management.md 补 Vec 持久化 + 乐观更新回滚两条 convention。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `20c979c` | (see git log) |
| `c52104b` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 6: WSL 项目 envVars 通过 WSLENV 透传 + 升级 v0.4.15

**Date**: 2026-05-26
**Task**: WSL 项目 envVars 通过 WSLENV 透传 + 升级 v0.4.15
**Branch**: `main`

### Summary

后端 pty.rs 抽出 build_wslenv_value 纯函数，WSL 分支拼 WSLENV=K1/u:K2/u 并把宿主既有值追加在尾部合并，对齐 JetBrains IDEA terminal 惯例；MINITERM_ 前缀 + WSLENV 大小写敏感等值前后端双重防御过滤；前端 ProjectEnvVarsModal 拒绝 WSLENV 作为 key，WSL 顶部警告条由黄变绿；新增 7 个 build_wslenv_value 单测覆盖空 list / 单变量 / 多变量顺序 / 宿主合并 / 空字符串边界；spec pty-env-vars-injection.md WSL 章节从 v2 预留升级为已实现；版本号 0.4.14 → 0.4.15。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `53f97fb` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 7: fix Windows 深色模式标题栏 (issue #33)

**Date**: 2026-05-26
**Task**: fix Windows 深色模式标题栏 (issue #33)
**Branch**: `main`

### Summary

调 Win32 DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE=20) 切原生标题栏配色，挂在 themeManager.applyToDOM 末尾，theme 切换 / 启动 / auto 系统色变化三处自动同步。Cargo windows crate 加 Win32_Graphics_Dwm feature；适配 Win10 20H2+ / Win11，非 Windows cfg 包裹 no-op，失败 eprintln 不阻塞。trellis-check 一次过，5fc8ccb 提交。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `5fc8ccb` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 8: xterm 终端连体字 (ligatures) 支持

**Date**: 2026-05-28
**Task**: xterm 终端连体字 (ligatures) 支持
**Branch**: `main`

### Summary

新增 @xterm/addon-ligatures 集成；AppConfig 加 terminal_ligatures 字段四层对齐 (Rust struct + serde + TS + store)；terminalCache 把 LigaturesAddon 必须先于 WebglAddon 加载这一约束硬编码 (绕过上游 #5455)，抽 4 个 addon 加载/dispose helper，新增 reloadLigaturesForPty 同步无 await 重做链路避 pty-output race；TerminalInstance useEffect 监听切换触发已开 pane 热重做；设置-字体页加开关与平台差异说明；新增 frontend spec xterm-ligatures-with-webgl-order.md 沉淀加载顺序/热切换/平台差异约束。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `0b31a35` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 9: 修复 xterm.js WebGL 共享 atlas 致多 claude 终端同时乱码

**Date**: 2026-05-28
**Task**: 修复 xterm.js WebGL 共享 atlas 致多 claude 终端同时乱码
**Branch**: `main`

### Summary

深入诊断 xterm.js addon-webgl CharAtlasCache 跨终端共享导致的 vertex buffer 失效 → 多 claude 并发出现同形乱码。修复在 loadWebgl 内挂 onAdd/onRemoveTextureAtlasCanvas 广播 term.refresh 唤醒 dormant render loop;归档 prd+完整证据链 research+spec(xterm-webgl-atlas-sharing.md) 供未来 upgrade addon-webgl 时回归。fix/xterm-shared-atlas-mojibake 分支 cherry-pick 到 main,与 ligatures 任务合并

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `9bb05e4` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 10: mini-term × cc-connect 集成 (PR1+PR2+PR3+spec)

**Date**: 2026-05-28
**Task**: mini-term × cc-connect 集成 (PR1+PR2+PR3+spec)
**Branch**: `feat/cc-connect-panel`

### Summary

完成 mini-term 与 cc-connect 端到端集成三件套:进程启停管理 + 项目导入与关联 + dashboard iframe 嵌入。PR1 基建层新增 src-tauri/src/cc_connect.rs 含 8 个 Tauri command (probe/read_token/start/stop/restart/list_projects/import_project/unlink_project),走 ureq 调本机 Management API :9820,import 用 toml_edit 写回 [[projects]] 保留注释 + POST /api/v1/restart (cc-connect 无 reload 路径让新项目生效),AppConfig 扩 ccConnect?: CcConnectConfig 字段,5 个单元测试 round-trip;PR2 UI 入口 SettingsModal 加 cc-connect 栏 (4 字段 + 5 按钮 + 状态指示器) + App.tsx 标题栏 CcConnectStatusDot 三态 + autoStart 钩 mount + useCcConnectProbe 5s 轮询失焦暂停;PR3 ProjectList 右键 4 项 (导入/解除/配置平台/清理失效关联) + 项目 icon 绿◆/红⚠ race-safe 关联状态 + CcConnectDashboard 全屏 iframe createPortal 到 document.body 绕 Fluent 2 backdrop-filter containing block + keep-alive display:none 避免重 login;沉淀 4 项 spec (toml-edit-array-of-tables / cc-connect-integration / tauri-command-nested-args / fluent2-portal-modal)。check sub-agent 抓出并修复 race 误报红 + URL 未编码 + probe useEffect 反复 fire 三个真 bug。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `18e7e9d` | (see git log) |
| `d10ce72` | (see git log) |
| `0bc7be0` | (see git log) |
| `4a308c6` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 11: cc-connect-panel review 3 Critical + Major #7 修复

**Date**: 2026-05-29
**Task**: cc-connect-panel review 3 Critical + Major #7 修复
**Branch**: `feat/cc-connect-panel`

### Summary

code review feat/cc-connect-panel 分支后修复 4 个 ship 必须修的问题:Critical #1 CcConnectDashboard 双挂载 → store 加 dashboard slice + App.tsx 单例挂载 + ProjectList 删 local state;Critical #2 cc-connect restart 后 iframe 不刷新 → lastSeenRunning/lastSeenOwnPid ref 边缘检测,running false→true 或 ownPid 变化时强制 rebuild,true→false 保留 url 避免临时掉线白屏;Critical #3 restart fallback exe_path 缺失导致 child 已杀但未 spawn 的半同步态 → exe_path 校验前置到 take child 之前;Major #7 import_project/unlink_project 半同步态 → 返回值改 ImportProjectResult/UnlinkProjectResult struct,toml/DELETE 成功后 restart 失败编码到 result 不返 Err,前端仍写/清 projectLinks + 警告 toast 消除项目存在但未关联或项目已删 icon 仍红场景。spec cc-connect-integration.md Validation Matrix 同步契约。cargo test 154 / cargo check / tsc / vite build 全 clean。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `6bacb19` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 12: README + Cargo.toml 文档同步 cc-connect 集成

**Date**: 2026-05-29
**Task**: README + Cargo.toml 文档同步 cc-connect 集成
**Branch**: `feat/cc-connect-panel`

### Summary

feat/cc-connect-panel 分支文档收尾:README 顶部 slogan 加 '· IM 平台远程驱动 (cc-connect)' 亮点,功能特性新增 '### cc-connect 集成' 分组放在 AI 进程感知后/项目管理前,涵盖进程管理(启停/测试连接/autoStart/状态点三态) / 项目导入与关联(toml_edit 写盘+restart+hash 冲突后缀+race-safe icon) / Dashboard 嵌入(iframe 自动登录+createPortal 绕 Fluent 2+keep-alive) / 半同步态处理(tomlWritten 与 deletedOk 分支) / 优雅降级 五大子项。src-tauri/Cargo.toml description 从占位符 'A Tauri App' 改成 mini-term 真实描述,对齐 mt-core/Cargo.toml 既有风格。本会话不归档新 task(本轮无 active task)。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `16b2248` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 13: cc-connect 一键导入集成并入主线 + v0.4.21 发版

**Date**: 2026-05-29
**Task**: cc-connect 一键导入集成并入主线 + v0.4.21 发版
**Branch**: `main`

### Summary

「连接」弹窗内置项目导入到 cc-connect：支持单个/勾选批量一键导入，已关联项可解除、失效项可清理。新增后端 cc_connect_import_projects 命令（批量一次写盘 + 仅重启一次 cc-connect），现有项目与批次内统一去重（冲突加 hash 后缀）。探活改为常驻、放宽右键导入门控支持零配置识别运行态。修复 Windows 启动失败/孤儿进程/卡顿，移除会产出无效配置的项目导入。确认框支持换行与超长滚动折叠。最终升级版本号至 v0.4.21，首次将 cc-connect 集成并入主线。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `d06df9c` | (see git log) |
| `6cb688d` | (see git log) |
| `50b5cfc` | (see git log) |
| `4a5c294` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 14: 归档全应用中英双语 (i18n) 任务

**Date**: 2026-06-01
**Task**: 归档全应用中英双语 (i18n) 任务
**Branch**: `main`

### Summary

完成并归档 i18n 双语任务:自研 zustand i18n 基础设施(useT/t、点分 key、{param} 插值、localStorage 持久化+navigator 自动探测),28 个组件文案抽取为 src/i18n/locales 下 29 个 zh/en 命名空间字典,新增 LanguageToggle 接入设置→系统分页;随 v0.5.0 发布。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `59926aa` | (see git log) |
| `591c598` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 15: 全项目深度 review + 修复 search 崩溃/clipboard 越界/配置原子写/资源泄漏

**Date**: 2026-06-01
**Task**: 全项目深度 review + 修复 search 崩溃/clipboard 越界/配置原子写/资源泄漏
**Branch**: `main`

### Summary

对 mini-term 全 17 模块做多 agent 对抗式深度 review(确认 22 问题、推翻含 rehype-raw XSS 在内的 4 误报);在 worktree 分支 fix/review-2026-06-01 修复并对抗复审通过:H1/H2 搜索遇 CJK/多字节字符崩溃且前端永久卡死(重写子串匹配为逐字符折叠+catch_unwind)、H3 剪贴板 DIB 越界读/整数溢出、配置文件原子写横切(16 处 fs::atomic_write,含 Unix 权限位保留)、删项目与 FileTree watcher 的 PTY/终端/句柄泄漏。164 Rust 测试+tsc 全绿;分支未合并,剩余 medium/low 入 backlog。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `f1d187e` | (see git log) |
| `1fcf1bc` | (see git log) |
| `d210ce7` | (see git log) |
| `b28c4dd` | (see git log) |
| `571782d` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 16: 修复 mt-ssh-mcp 无法用传统 PKCS#1 RSA 私钥连接 SSH

**Date**: 2026-06-06
**Task**: 修复 mt-ssh-mcp 无法用传统 PKCS#1 RSA 私钥连接 SSH
**Branch**: `main`

### Summary

诊断 Oracle Cloud SSH 连接失败(系统 ssh 同 key 可登, mt-ssh-mcp 报 Unsupported key type RSA)。定位两个串联的坑: ① russh 底层 ssh-key 0.7 不认传统 PKCS#1 明文 RSA PEM; ② PrivateKeyWithHashAlg::new(key,None) 对 RSA 落到 SHA-1, 被现代 OpenSSH(>=8.8)拒。修复: pool.rs 加纯 Rust PKCS#1 PEM->DER fallback(load_private_key_compat/try_parse_pkcs1_rsa) + authenticate 按 server-sig-algs 选 rsa-sha2-512(is_rsa 门控); Cargo.toml 增 ssh-key(rsa feature)/rsa 依赖, 版本锁定与 russh 一致、无 aws-lc、rand_core 统一。验证: 临时 example 端到端实连 oracle-4c-24g 成功 + 全量 cargo test/clippy + debug/release 构建 + 双 agent high-effort review 修 3 项 quality。固化 spec russh-rsa-key-loading.md(含可执行契约)。生效需重启 mini-term。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `6cda88d` | (see git log) |
| `318e4f6` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 17: mt-ssh-mcp SFTP 文件上传/下载

**Date**: 2026-06-09
**Task**: mt-ssh-mcp SFTP 文件上传/下载
**Branch**: `feat/ssh-mcp-sftp-transfer`

### Summary

为 mt-ssh-mcp 接入 russh-sftp 2.3.0,新增 ssh_upload/ssh_download 两个 MCP 工具:大文件流式分块、config.json 明文密码外泄硬护栏、协议层 set_timeout 10s 逐请求超时修复;复核依赖零 aws-lc 不破坏精确锁。固化 russh-sftp-file-transfer.md 后端 spec(7 段契约),README 中英双版更新。全程走 trellis 流程:brainstorm→research(russh-sftp 兼容性)→implement→check(审出并修复协议层超时缺陷)→spec→commit。66 单测全绿,clippy 零警告。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `43cf5e6` | (see git log) |
| `05d182d` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 18: Session 块支持查看 WSL 内 claude/codex 历史会话

**Date**: 2026-07-02
**Task**: Session 块支持查看 WSL 内 claude/codex 历史会话
**Branch**: `feature/wsl-ai-sessions`

### Summary

grilling 拷问收敛 7 项需求决议后走完 trellis 全程：WSL 根项目（\wsl$ UNC）自动加载发行版内会话零配置；Windows 路径项目右键「WSL 会话」子菜单选发行版（注册表 Lxss 枚举，不 spawn wsl.exe），按 /mnt 规则映射路径；Session 列表分段加载合并混排带 WSL 标识与竞态防护；会话正文查看回传 wslDistro；WSL 侧扫描限额/独立缓存/静默降级；沉淀 CONTEXT.md 术语表与 wsl-unc-session-scanning.md 后端规范；cargo test 176 通过、npm run build 通过

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `cea1250` | (see git log) |
| `c35b331` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete


## Session 19: SSH 远程开发：远程项目全链路（grilling 设计 + 三批实现 + 双轮检查）

**Date**: 2026-07-05
**Task**: SSH 远程开发：远程项目全链路（grilling 设计 + 三批实现 + 双轮检查）
**Branch**: `feature/ssh-remote-projects`

### Summary

grilling 访谈逐分支收敛 SSH 远程项目设计（类 Remote-SSH：sshConnectionId 引用连接 + 远程 POSIX path），trellis 全流程落地：PR1 抽 mt-ssh 共享 crate（russh 池+SFTP 原语，sidecar 零行为变化）；PR2 后端四个 ssh_remote_* async command + create_pty sshRemote 直连启动器（引号安全、autofill spawn 前预注册）；PR3 前端添加入口/远程文件树/断线一键重连/远程 Session 混排，i18n 双语。两轮 trellis-check 修复 5 问题（map_while 截断回归、exitedPtyIds 无界增长、缓存串键等）。cargo test 209+26+40 全绿，tsc 通过。spec 新增 ssh-remote-project.md 契约，CONTEXT.md 补术语。README 留发版更新；真机端到端验收待做。

### Main Changes

(Add details)

### Git Commits

| Hash | Message |
|------|---------|
| `413b8f1` | (see git log) |
| `b878b52` | (see git log) |
| `d8bb2d9` | (see git log) |
| `bdbfaa4` | (see git log) |

### Testing

- [OK] (Add test results)

### Status

[OK] **Completed**

### Next Steps

- None - task complete

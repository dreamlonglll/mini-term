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

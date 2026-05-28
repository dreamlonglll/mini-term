# brainstorm: mini-term × cc-connect 集成

## Goal

让 mini-term 用户在不离开 mini-term 的前提下,完成 cc-connect 的端到端体验。用户最终需求三件套:

1. **进程管理**: mini-term 设置里有 "cc-connect 集成" 块,填可执行文件路径、config 路径、启动参数,按钮启停/重启 cc-connect 进程
2. **项目导入与关联**: 把 mini-term 的项目(ProjectList 里的)一键导入到 cc-connect `[[projects]]`,并把 "mini-term project ↔ cc-connect project name" 关联存在 mini-term 自己 config 里
3. **Dashboard 嵌入**: 在 mini-term 内嵌 cc-connect 自家 Web Dashboard (`http://localhost:9820/login?token=xxx`),用户在 mini-term 里直接给每个项目配 IM 平台(不自己写 platform 表单,完全复用 cc-connect 现成 wizard)

## What I already know (Research findings)

### cc-connect 接入面 ── 已现场验证

- Management API `http://0.0.0.0:9820/api/v1/*`,Bearer token 在 `~/.cc-connect/config.toml` `[management].token`
- 本机 cc-connect **v1.3.2 在跑**(PID 50016,监听 9820 + 9810),`[management].enabled = true`,`[bridge].enabled = true`,`cors_origins = ["*"]`
- 本机已经有 `[[projects]] name = "Mini-term"` 条目,`agent.type = "claudecode"`,带 `[projects.agent.options]` 子段 ── 说明用户之前手动配过

### 项目创建路径 ── 已验证

- **没有** `POST /api/v1/projects` 创建端点
- `POST /api/v1/projects/{name}/add-platform` 是 **upsert 语义**(项目不存在就创建并写 toml),但必须一并给一个 platform body
- 走 toml_edit 改 config.toml + `POST /api/v1/restart` 是事实标准路径
- ⚠ `POST /api/v1/reload` 当前 main 分支**对全新项目不生效**(只遍历已注册的 engine map);**全新项目必须 restart**
- ⚠ restart 会**断开所有 active sessions**,turn 中断,但 chat 历史保留

### Dashboard 嵌入 ── 已验证可行

- cc-connect web `Login.tsx` 支持 `?token=xxx` URL 参数自动登录
- 无 `X-Frame-Options` / CSP `frame-ancestors`,Tauri WebView2 可嵌
- API 走相对路径 `/api/v1`,iframe 内同源调用无 CORS 问题

### Windows 进程管理 ── 已验证

- `cc-connect daemon install` 在 Windows 需 v1.3.3+(用 Task Scheduler);本机 v1.3.2 不支持
- mini-term 自己 spawn 方案:**复用现有 PTY** 即可,无需引 `tauri-plugin-shell` / `shared_child`,可在普通终端 tab 里跑 cc-connect 直观看日志
- 退出策略: mini-term 关闭时 **不 kill cc-connect**(IM 持续可用)

## Requirements (MVP)

### 模块 A: cc-connect 集成设置块

- 位置: SettingsModal 新增 "cc-connect" 栏
- 字段(持久化到 `AppConfig.ccConnect`):
  - `exePath`(默认: 自动探测 `where cc-connect`)
  - `configPath`(默认: `%USERPROFILE%\.cc-connect\config.toml`)
  - `autoStart`(bool, mini-term 启动时是否自动起 cc-connect)
  - `extraArgs`(string[], 可选启动参数)
- 按钮:
  - **启动 / 停止 / 重启**(重启优先调 `POST /api/v1/restart`,fallback 到 kill+spawn)
  - **测试连接**(`GET /api/v1/status`,显示 token 有效 + 端口可达)
  - **打开 config.toml**(系统默认编辑器打开,方便高级用户)
- 状态显示: ● running(含 PID/端口/版本)/ ○ stopped / ⚠ error

### 模块 B: 项目导入与关联

- ProjectList 右键菜单加 "导入到 cc-connect" / "解除 cc-connect 关联"
- Project 详情 modal 加 "cc-connect 关联" 区块,显示当前状态 + 操作按钮
- 导入流程:
  1. 检查 cc-connect 是否在跑;没跑提示先去模块 A 启动
  2. 默认项目名 = mini-term project name(冲突时加 hash 后缀 `MyProj_a1b2c3d4`)
  3. Rust 端用 `toml_edit` 往 config.toml 写最小 `[[projects]]`:
     ```toml
     [[projects]]
     name = "<computed>"
       [projects.agent]
       type = "claudecode"  # MVP 默认,后续在 dashboard 改
         [projects.agent.options]
         work_dir = "<mini-term project.path>"
     ```
  4. `POST /api/v1/restart` 让新项目生效;UI **必须显式提示**"会重启 cc-connect,短暂中断 IM 连接,继续吗?"
  5. 在 `AppConfig.ccConnect.projectLinks[projectId] = ccProjectName` 持久化关联
- ProjectList item 右侧 cc-connect 小图标: 已关联高亮 / 未关联灰 / 关联失效(cc-connect 那边已删)红色 ⚠
- 解除关联: `DELETE /api/v1/projects/{name}` + 删 projectLinks 条目 + restart

### 模块 C: cc-connect Dashboard 嵌入

- 入口 1: ProjectList item 右键 → "在 cc-connect 配置平台"(直接跳到该项目的平台配置 deep-link)
- 入口 2: 顶部状态栏 indicator 点击 → 打开 "cc-connect Dashboard" 全屏 modal / Tab
- 实现:
  - `<iframe src="http://127.0.0.1:9820/login?token=<token>&redirect=/projects/<name>" />`
  - 容器组件 keep-alive,关闭只 hide 不 unmount,避免每次重 login + 重 fetch
  - 如果 dashboard URL/路由 hash 找不到 fallback 到 `/projects` 列表页

## Acceptance Criteria

- [ ] mini-term 启动时自动 probe cc-connect 状态,顶部状态栏可视化(running/stopped/error)
- [ ] 用户能在设置里启动/停止/重启 cc-connect,无需开 PowerShell
- [ ] 右键 mini-term 项目 → "导入到 cc-connect" → 在 cc-connect dashboard 能看到该项目
- [ ] 关联后 mini-term project 旁有 cc-connect 图标
- [ ] 解除关联会同步删除 cc-connect 项目并刷新 mini-term UI
- [ ] 点 "在 cc-connect 配置平台" 直接进对应项目页(已登录态)
- [ ] cc-connect 未启动时所有相关 UI 优雅降级提示"先启动",不抛错弹窗
- [ ] config.toml 写入**保留用户已有注释和顺序**(用 toml_edit,不用 BurntSushi/toml)
- [ ] mini-term 关闭时不 kill cc-connect(IM 持续可用)
- [ ] restart 操作前**必须显式 confirm**,避免静默断 sessions

## Out of Scope (显式排除)

- 自己实现 IM 平台配置表单 → dashboard iframe 接管
- 自己实现 IM 消息流视图 → dashboard iframe 接管
- 自己实现 cron / provider / session 管理 → 同上
- 给 cc-connect 上游提 PR 注册 mini-term 为 agent kind
- 自动下载/安装/升级 cc-connect 二进制(让用户自己 `npm i` 或 GitHub release)
- 多 cc-connect 实例切换(MVP 单实例)
- 导入时同时配置 platform(让用户进 dashboard 配)

## Decision (ADR-lite)

**Context**: 用户三件套需求(启停/导入/dashboard)。Research 显示 cc-connect 自家 dashboard 可 iframe 嵌入,Tauri WebView2 无障碍;创建项目无 API,必须 toml_edit + restart。

**Decision**:
1. UI 三段式: 原生设置块 + 原生 ProjectList 关联指示 + iframe 嵌入 dashboard 接管复杂表单
2. 进程管理走 Rust 端 spawn,复用现有 PTY 能力,不引 `tauri-plugin-shell` / `shared_child`
3. 创建/删除项目走 `toml_edit` 改 config.toml + `POST /api/v1/restart`,UI 必须 confirm
4. mini-term 关闭不联动关 cc-connect

**Consequences**:
- ✅ 开发量集中可控,UI 自然原生,复杂功能交给上游
- ✅ cc-connect 升级,iframe 自动跟上,几乎零维护
- ⚠ 导入/解除关联需 restart,会断 active sessions(必须 confirm)
- ⚠ cc-connect 升级若改 dashboard 路由 hash,iframe deep-link 会失效(需 fallback)
- ⚠ iframe 风格不和 mini-term 主 theme 融合(Fluent 2 下尤其明显);MVP 接受

## Implementation Plan (3 PRs)

**PR1 基建 (Rust + 类型)**
- `src-tauri/src/cc_connect.rs`: 8 个 Tauri command
  - `cc_connect_probe()` → `{ running, port, version, pid }`
  - `cc_connect_read_token()` → `String`(读 config.toml `[management].token`)
  - `cc_connect_start(exePath, configPath, args)` → `pid`
  - `cc_connect_stop()` → `()`(kill 进程)
  - `cc_connect_restart()` → `()`(POST /api/v1/restart fallback kill+spawn)
  - `cc_connect_list_projects()` → `Vec<{ name, work_dir, agent_type, has_platform }>`
  - `cc_connect_import_project(name, work_dir, agent_type)` → `()`(toml_edit + restart)
  - `cc_connect_unlink_project(name)` → `()`(DELETE + restart)
- 引依赖: `toml_edit = "0.22"`、`reqwest`(待查 Cargo.toml 是否已有)
- 前端 `types.ts` 扩 `AppConfig.ccConnect`,`store.ts` 加 `ccConnectStatus` slice + 5s probe 轮询

**PR2 模块 A + 状态栏**
- SettingsModal 加 "cc-connect" 栏
- 顶部状态栏 indicator 组件(点击跳设置)
- autoStart 钩 App.tsx mount

**PR3 模块 B + 模块 C**
- ProjectList 右键菜单 + 项目 modal cc-connect 区块
- `CcConnectDashboard` 组件(iframe keep-alive)
- 项目 icon 三态(关联/失效/未关联)

## Technical Notes

- 分支: `feat/cc-connect-panel`
- 任务目录: `.trellis/tasks/05-28-embed-cc-connect-panel/`
- Research 文件(persist 在 `research/`):
  - `cc-connect-http-api.md` — API 表面
  - `cc-connect-web-ui-patterns.md` — iframe 可行性
  - `config-toml-schema.md` — TOML round-trip
  - `cc-connect-process-supervision-windows.md` — 进程管理
  - `cc-connect-project-creation.md` — 创建路径(restart 必需)
- token 动态从 `~/.cc-connect/config.toml` 读,不写死
- Tauri WebView2 嵌 iframe 在 Windows 已验证可行
- mini-term 现有 spawn 模式: `pty::create_pty`(`src-tauri/src/pty.rs`)── 可用于"启动 cc-connect 到一个可见 tab"模式

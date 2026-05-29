# PRD：「连接」弹窗内一键导入项目到 cc-connect

## 背景与根因（务必先读，避免重蹈覆辙）

历史上 commit `d06df9c` 实现过「弹窗内勾选批量导入」，但 `6cb688d` 把它整体删除，理由是
**导入只写 `name/agent/work_dir`、不写 `[[projects.platforms]]`，产出无法冷启动的配置**。

本任务经多 agent 调研 + 隔离实测（cc-connect v1.3.2 @ 19406df）**证实并定位了根因**：

- `config/config.go` 的 `validate()`（L1097-1103）对**任意一个** `platforms` 为空的项目直接
  `return error`；`config.Load()` 冒泡到 `cmd/cc-connect/main.go` L234-237 → **`os.Exit(1)`**。
  失败发生在「配置校验阶段」，根本到不了 per-project engine 构建，是「一坏全退」。
- 实测：无 platform 项目启动 <1s 退出，stderr：`config: projects[N] needs at least one [[projects.platforms]]`。
- cc-connect **没有** `enabled=false`/`disabled` 之类「登记但不连」开关（`PlatformConfig` 只有 `{type, options}`）。

**结论：被删 commit 的判断是对的，但修法应是「补占位平台」而非删功能。**

## 方案（已与用户拍板）

- **占位平台·零摩擦导入**：后端 `make_project_table` 写 `[[projects]]` 时强制追加一个**冷启动安全**的
  占位 `[[projects.platforms]]`：`type="telegram"`，`options.token` 为**非空**假值。
  - 依据：telegram 工厂仅校验 token 非空，`Start()` 把拨号丢进 goroutine 后 `return nil`，
    假 token 只会后台退避重连、**绝不返回 error 让进程崩**（platform/telegram/telegram.go L137-198）。
  - **禁用 discord 占位**：其 `Start()` 同步 `session.Open()` 返回 error，单平台时拖垮 engine→`os.Exit`。
  - token 必须非空：空串会让 telegram 工厂 `os.Exit`。用可识别值 `0:MINITERM_PLACEHOLDER_REPLACE_IN_DASHBOARD`。
- **入口集中在 CcConnectModal**：状态/Dashboard 卡片下方新增「导入项目到 cc-connect」区块，
  列出全部 mini-term 项目，勾选/全选 → `一键导入(N)`；逐行也可单独「导入」。
- **保留「移除」**：已导入项提供「移除」按钮（`cc_connect_unlink_project`：DELETE + restart），用于纠错/撤销。
- **不做关联失效检测**：丢弃 `useCcConnectProjects` 的 10s 轮询 / `missingLinks` / `listLoaded` race-safe / 失效红标。
  「已导入」态直接以本地 `projectLinks[id]` 是否存在为准。

## 用户流程

1. 顶栏「连接」打开弹窗 → 未运行先点「启动」（零配置回退 PATH + `~/.cc-connect/config.toml`）。
2. 导入区块勾选项目（或全选）→ `一键导入(N)` → confirm（提示带占位 Telegram 平台、稍后去 Dashboard 换真平台、会重启断 IM）。
3. 后端一次写盘所有 `[[projects]]`（各含占位平台）+ 只 POST 一次 `/restart`。
4. 写 `projectLinks`、刷新 status，对应行翻「● 已导入」。
5. （可选）「打开 Dashboard」把占位 telegram 换成真实 IM 平台。

## 已知风险 / 边界

- **占位脏**：导入项目在 Dashboard 显示为「连不上的 Telegram 平台」，会后台退避重连，直到用户替换/删除。
- **502 风险**：占位项目处于 pending（`ready=0`）时 `/api/v1/status` 返 502。
  → 在 cc-connect **尚未配过任何真实平台的首次场景**（全占位）下，探活可能整体变红（进程其实活着）。
  导入成功判定**以 `tomlWritten` 为准，不依赖 `status.running`**。一旦有 ≥1 真实平台 ready，status 预期恢复。
- 重名：前端 `cc_connect_list_projects` + 批次内部统一去重，冲突加 8 字符 hash 后缀；后端 `existing` HashSet 二次防御。
- restart 失败半同步态：`tomlWritten=true` 即写 `projectLinks` + 警告「下次启动生效」，不卡在 `restartOk`。
- 用户若在 Dashboard 删占位平台又不加真平台 → 该项目又变 platform-less，下次重启崩（cc-connect 上游行为）。confirm 文案告知「请替换而非直接删除占位平台」。

## 改动面

- **后端** `src-tauri/src/cc_connect.rs`：`make_project_table` 注入占位平台 + 两个 const；单测加一条断言产物含 platform。
- **前端** `src/types.ts`：恢复 `CcProject`/`ImportProjectRequest`/`ImportProjectResult`/`BatchImportResult`/`UnlinkProjectResult`。
- **前端** `src/utils/ccConnectImport.ts`（新）：复用被删 `ccConnectActions.ts`，去掉 `refreshCcProjects`/关联语义，confirm 文案改占位平台版。
- **前端** `src/components/CcConnectModal.tsx`：状态卡片下插入导入区块（两态：未导入=勾选+导入 / 已导入=●已导入+移除）。
- **不动** `ProjectList.tsx`、`lib.rs`（4 命令仍注册）、restart 策略。

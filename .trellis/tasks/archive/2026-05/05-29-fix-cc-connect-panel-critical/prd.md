# fix: cc-connect-panel review critical follow-up

## Goal

修复 `feat/cc-connect-panel` 分支 code review 出的 3 个 Critical + 1 个 Major(与 Critical #1 耦合),让分支可以 ship。

## Scope (4 个修复点)

### Critical #1: CcConnectDashboard 双挂载 → 提到 App 顶层单例

**现状**: `src/App.tsx:393` 和 `src/components/ProjectList.tsx:669` **各自**挂一份 `<CcConnectDashboard>`,各自持本地 `ccDashboardOpen` / `ccDashboardDeepLink` state。

**问题**: 顶栏点开 + 右键再点 → 两个独立 iframe 各自 keep-alive → 内存 ×2 + z-index 50 重叠 + Esc 行为不确定。

**修法**:
- 在 `src/store.ts` 加 dashboard slice:`ccDashboardOpen: boolean` / `ccDashboardDeepLink: string` + actions `openCcDashboard(deepLink?: string)` / `closeCcDashboard()`
- `App.tsx` 单例挂 `<CcConnectDashboard open={ccDashboardOpen} onClose={closeCcDashboard} deepLink={ccDashboardDeepLink} />`,删 local state
- `ProjectList.tsx` 删 local state 和挂载,右键 "在 cc-connect 配置平台" 直接 `openCcDashboard(/projects/<name>)`
- `CcConnectStatusDot` 双击 / "Dashboard" 按钮也走 `openCcDashboard()`(无 deepLink)

### Critical #2: cc-connect restart 后 iframe 不刷新

**现状**: `CcConnectDashboard.tsx:34-65` `useEffect` 有 `if (iframeUrl && !deepLinkChanged) return;` 跳过 rebuild;iframeUrl 一旦设置后,cc-connect 重启(token/port 不变但 session 失效)iframe 看空白/登录页。

**修法**:
- 在 `CcConnectDashboard.tsx` 内部 ref / state 追踪 `lastSeenRunning: boolean` 与 `lastSeenOwnPid: number | undefined`
- `useEffect` 监听 `status.running` false→true 边缘 **或** `status.ownPid` 变化 → 强制 `setIframeUrl(null)` 触发下次 buildUrl
- 边缘检测保持简单,不引新 store state

### Critical #3: restart fallback exe_path 缺失导致半同步态

**现状**: `cc_connect.rs:294-327` `cc_connect_restart` HTTP fallback → take child kill → `cc_connect_start(exe, ...)`;若 `exe_path` 为 None,child 已杀但未 spawn,**state 已清空**,用户重试 stop 提示"不是 mini-term 启动",必须手动重启 cc-connect。

**修法**:
- HTTP restart 失败后,**先校验** `exe_path.is_some()` 才 take + kill;`exe_path = None` 直接返 Err "HTTP restart 失败,且未提供 exe_path 用于 fallback 重启"
- 错误信息明确告诉用户后续如何手动恢复

### Major #7: import_project 半同步态(toml 写成功但 restart 失败)

**现状**: `cc_connect.rs:372-420` `cc_connect_import_project` 在 toml 写成功后 POST /restart;若 restart 失败,**Rust 端返 Err**,前端 `ccConnectActions.ts:104` 抛错 → **不写 projectLinks**;cc-connect config.toml 已有项目但 mini-term 没记关联 → UI 显示"未关联",但下次重试导入会撞 duplicate 检测死循环。

**修法**:
- 后端 `cc_connect_import_project` 返 struct `ImportProjectResult { name, tomlWritten: bool, restartOk: bool, restartError?: String }`,而非 `()`
- 前端 `ccConnectActions.ts` 根据返回值:
  - `tomlWritten && restartOk` → 写 projectLinks + 成功 toast
  - `tomlWritten && !restartOk` → 仍写 projectLinks + 警告 toast "导入成功但 cc-connect 重启失败 (<reason>),下次启动 cc-connect 会生效"
  - `!tomlWritten` → 抛错(toml 写失败的原始错误)
- 同理处理 `cc_connect_unlink_project`:返 `{ name, deletedOk, restartOk, restartError? }`,前端按 deletedOk 决定是否删 projectLinks

## Non-goals (本 task 不做)

- Major #4-#10(Mutex poison / hash 注释 / Windows kill 优雅 / list 静默 / 单双击 / configPath rebuild)── 留下个 polish PR
- 所有 Minor / Info
- 新功能

## Acceptance Criteria

- [ ] 顶栏开 dashboard + ProjectList 右键开 dashboard,**只有一份 iframe** 在 DOM 里(用 devtools 验证),Esc 行为确定
- [ ] cc-connect 重启后 iframe 自动刷新到 login 页(`status.running` 边缘或 `ownPid` 变化触发)
- [ ] `cc_connect_restart` 在 `exe_path=None` 且 HTTP 失败时返清晰 Err,不杀 child;child 仍在管理中,后续 stop 仍能正常工作
- [ ] import_project 在 toml 已写但 restart 失败时,mini-term 仍写 projectLinks,UI 不出现"项目存在但未关联"半同步态;toast 提示用户重启 cc-connect 生效
- [ ] unlink_project 同理:DELETE 成功但 restart 失败时仍删 projectLinks
- [ ] `cargo test --lib cc_connect` 全过(可能需要更新单测匹配新返回 struct)
- [ ] `npx tsc --noEmit` clean
- [ ] `npx vite build` clean

## Technical Notes

- 分支: 继续在 `feat/cc-connect-panel` 上(不另开)
- 涉及文件预估:
  - `src-tauri/src/cc_connect.rs`(Critical #3 + Major #7,改 restart 与 import/unlink 返回值)
  - `src-tauri/src/lib.rs`(若新增 ImportProjectResult struct 在 cc_connect.rs 内即可,lib.rs 无需改)
  - `src/types.ts`(加 ImportProjectResult / UnlinkProjectResult interface)
  - `src/store.ts`(Critical #1 加 dashboard slice)
  - `src/App.tsx`(Critical #1 单例挂载)
  - `src/components/ProjectList.tsx`(Critical #1 删 local state)
  - `src/components/CcConnectDashboard.tsx`(Critical #1 接 store + Critical #2 running/ownPid 边缘 rebuild)
  - `src/utils/ccConnectActions.ts`(Major #7 按返回值分支)
- review 报告: 见上轮对话主线
- 上游 task: `.trellis/tasks/archive/2026-05/05-28-embed-cc-connect-panel/`

## Decision (ADR-lite)

**Context**: code review 出 3 Critical + 7 Major,本 task 只修能让分支 ship 的子集(3 Critical + 1 关键 Major)。

**Decision**: 跳过 brainstorm Q&A(需求已被 review 明确);单一 fix task,继续 `feat/cc-connect-panel` 分支累积 commit;采用 review 报告里的建议修法。

**Consequences**:
- ✅ Critical 全清后分支可 ship 进 main
- ✅ Major #7 顺带消除"重试死循环"的差体验
- ⚠ 其余 Major / Minor 不修,作为已知 follow-up 在 PR description 列出
- ⚠ ccConnectActions API 形态变化(返 result struct),调用方需同步更新,但仍在本分支内闭环

# feat: Hook Server 按需启动

## Goal

将 hook HTTP server 从"app 启动时无条件启动"改为"由用户在设置页面控制是否启动"。核心动机：Windows 上 `tiny_http` 监听端口会触发防火墙授权弹窗，不应在用户未知情的情况下弹出。

## Requirements

- `config.json` 新增 `hookEnabled: bool` 字段，默认 `false`
- App 启动时读取配置，仅当 `hookEnabled == true` 时才启动 hook server
- 新增 Tauri command `toggle_hook_server(enabled: bool)`，支持运行时启动/停止 server
  - 启动：绑定端口、启动监听线程、写入端口文件
  - 停止：调用 `server.unblock()` 中断阻塞循环、释放端口、清理端口文件
- `Server` 实例存入 `HookState`（`Arc<Mutex<Option<Server>>>`），供运行时停止
- Hook 注册/卸载（`register_ai_hooks` / `unregister_ai_hooks`）与 server 开关保持独立，用户分别操作
- 设置页面 Hook 管理区域顶部新增 toggle 开关
  - 开关状态绑定 `hookEnabled` 配置项
  - 开关关闭时，下方注册/卸载按钮、服务器状态、配置片段展示整块置灰（`opacity + pointer-events: none`）
  - 切换开关时调用 `toggle_hook_server` command 并同步保存配置

## Acceptance Criteria

- [ ] 全新安装：app 启动后 hook server 不启动，`127.0.0.1:23456` 无监听，无防火墙弹窗
- [ ] 设置页面打开 Hook 开关 → server 启动，状态指示变为"运行中"
- [ ] 设置页面关闭 Hook 开关 → server 停止，端口释放
- [ ] 关闭开关后重启 app → server 不启动
- [ ] 打开开关后重启 app → server 自动启动
- [ ] Hook 注册/卸载按钮在开关关闭时置灰不可点击
- [ ] 运行时切换不影响已有 PTY 的进程监控（轮询降级正常工作）

## Definition of Done

- Rust 编译通过，无 Clippy 警告
- 前端 `npm run build` 类型检查通过
- 现有 81 个 Rust 测试全部通过
- 手动验证：开关切换、重启持久化、防火墙不弹窗

## Out of Scope

- 自定义端口号（保持 23456 起始 + 自动递增）
- Hook 注册/卸载与 server 开关联动（保持独立）
- 非 Windows 平台的防火墙处理

## Technical Approach

### 后端改动

1. **`config.rs`**：`AppConfig` 新增 `hook_enabled: bool`，`default()` 为 `false`，迁移兼容（旧配置无此字段时默认 `false`）
2. **`hook_server.rs`**：
   - `HookState` 新增 `server: Arc<Mutex<Option<tiny_http::Server>>>`
   - `start_hook_server` 改为返回值或直接操作 `HookState.server`
   - 新增 `stop_hook_server` 函数：取出 server → `unblock()` → 清除端口文件
   - 新增 `toggle_hook_server` Tauri command
3. **`lib.rs`**：`setup` 中读取 config，`hookEnabled == true` 时才调用 `start_hook_server`

### 前端改动

4. **`SettingsModal.tsx`**：Hook 区域顶部加 toggle，联动置灰下方内容，切换时调用 `toggle_hook_server` + `save_config`

## Technical Notes

- `tiny_http::Server` 实现了 `Clone`，调用 `unblock()` 可中断 `incoming_requests()` 阻塞循环
- 端口文件路径：`{app_data_dir}/hook-server.json`，停止时需删除
- 现有 `get_hook_status` command 已返回 `HookStatusInfo { port, running }`，`running` 字段可复用
- 涉及文件：`config.rs`、`hook_server.rs`、`lib.rs`、`SettingsModal.tsx`

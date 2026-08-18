# GPUI 迁移进度看板

> 本文档只记「迁到哪了」，迁移方案与技术决策见 [gpui-migration.md](./gpui-migration.md)。
> 由主会话在每个任务派出 / 验收 / 提交节点更新；标注均为当地时间。
>
> 状态图例：⬜ 未开始 · 🔵 进行中（agent 已派出） · 🟡 已交付待验收 · ✅ 已验收提交 · ❌ 受阻（附原因）

## 总览

| 阶段 | 内容 | 状态 |
|---|---|---|
| 骨架 | 工作区 9 crate + 依赖选型 + 迁移映射（`aa9a7fc`） | ✅ 2026-08-18 |
| Wave 1 | 后端五块并行搬运 + TerminalElement 端到端 | 🔵 2026-08-18 派出 |
| Wave 2 | mt-relay、mt-app 全壳（store/三栏/Tab/分屏树）、面板与 Modal、i18n、主题桥 | ⬜ 等 Wave 1 |
| Wave 3 | 五项验收（对齐/IME/选择剪贴板/拖拽/背景图与主题）、托盘、收尾清理 | ⬜ |
| 收尾 | mt-ssh/mt-core 移入 crates/、删 src-tauri/ 与 src/、发版切换 | ⬜ |

## Wave 1 —— 2026-08-18 派出 6 个并行 agent

| # | crate | 任务 | 来源（src-tauri/src/） | 状态 | 验收记录 |
|---|---|---|---|---|---|
| A | mt-usage | 用量统计整块去 Tauri 化 | usage_stats/{mod,turns,ledger,aggregate,pricing}.rs（~3.5k 行） | ✅ | 2026-08-18 主会话独立 target 复跑 58/58 绿（首轮挂的正是已知 flaky 并发测试，重跑即过）；turns/aggregate/pricing 仅 5 处路径/可见性差异；async 查询改同步、emit 改 SyncSink；临时 `ai_shim.rs` 待收编 |
| B | mt-config | 配置持久化 + 主题包；app_data_dir 改 dirs 拼接，保留 migrate_legacy_app_data | config.rs、theme_packs.rs | ✅ | 2026-08-18 主会话独立 target 复跑 41+1 doctest 全绿；已查证 dirs::data_dir()+identifier 与 Tauri v2 同磁盘位置（Roaming）并有测试钉住；ConfigToken→ConfigStore 字段、read_theme_asset 改返回 Vec<u8>；SshConnection 本地复刻防 mt-core 耦合 |
| C | mt-project | fs/git/search/editor/wsl_distros；emit 改注入回调；**不搬 remote_ssh** | fs.rs、git.rs、search.rs、editor.rs、wsl_distros.rs | ✅ | 2026-08-18 主会话独立 target 复跑 76/76 绿；FsWatcher 注入 sink、search_id 消失改 SearchHandle、editor 拆纯函数不依赖 mt-config、opener 改平台原生 spawn；顺修 get_worktree_branches 的 UNC 判断隐性 bug；⚠️ git 阻塞调用需调用方自己丢后台执行器 |
| D | mt-ai | hook 体系+状态判定+会话记录**逐字搬运**；StatusSink 注入；去重表与「降级结论落盘」铁律保留 | hook_server.rs、hook_registry.rs、process_monitor.rs、ai_sessions.rs、pty.rs 的 AI 识别段 | 🔵 | |
| E | mt-pty | conpty_bootstrap + pty.rs 存留部分；**公开 API 只增不改**；净删除三件套不搬 | pty.rs、conpty_bootstrap.rs | 🔵 | |
| F | mt-ui + mt-app | 自研 TerminalElement（逐 cell 绘制/宽字符对齐/默认背景不发 quad）+ 真实 PTY 端到端 demo | —（全新自研，替 xterm.js） | 🔵 | |

**并行纪律**（agent 提示词里已固化）：每个 agent 只写自己的 crate 目录；根 Cargo.toml 禁改；构建测试永远 `-p`；不自行 commit，由主会话验收后统一提交。

## Wave 2 —— 规划（派出时细化）

| 任务 | 依赖 | 说明 |
|---|---|---|
| mt-relay 搬运 | mt-ai（会话记录/镜像绑定判定） | mobile_relay.rs + mobile_mirror.rs；协议与 mobile/ 完全不动 |
| mt-app 全壳 | Wave 1 全部 | store（对应 src/store.ts）、三栏 resizable、Tab 栏、SplitNode 树、文件树接 mt-project、状态灯接 mt-ai |
| 面板与 Modal | mt-app 全壳 | 终端配置 / AI 历史 / 用量统计 / 移动端面板 / 分支树 |
| i18n + 主题桥 | mt-app 全壳 | rust-i18n 字典从 src/locales/*.ts 转；theme_packs 配色映射 gpui-component 主题层 |

## 已验收提交

（首个 Wave 1 交付验收后开始记录：commit、crate、测试结果、与原实现的偏差摘要）

## 技术债与待修清单（迁移期产生）

- **mt-usage/ai_shim.rs**：复制了 mt-ai 的 6 个纯函数（normalize_path / collect_codex_session_paths / codex_meta_from_line / codex_user_title_from_line / load_codex_thread_names / grok_home），等 mt-ai 验收后删文件改 `use mt_ai::{...}`；在此之前两份实现同步维护。
- **ledger `journal_mode=WAL` 的 BUSY 竞态是真缺陷**（迁移中定位，非迁移引入）：SQLite 对 journal-mode 转换不调 busy handler，首次启动时查询与后台同步同刻 open 非 WAL 库，输者报「账本打开失败: database is locked」。修法：`open_raw` 对该 pragma 按 busy_timeout 预算做 BUSY 限定重试。独立提交、独立 review，不混进迁移。
- Cargo.lock 已把 rusqlite/libsqlite3-sys pin 到与 src-tauri 完全一致（0.40.1/0.38.1）。
- **SshConnection 归属决议**：mt-config 内复刻了 mt-core 的同形结构（serde 形状有回归测试钉住）。决议：config 是 sshConnections 的持久化归属方，其他 crate 统一引用 `mt_config::SshConnection`；mt-core 移入 crates/ 后改 re-export。
- `atomic_write` 在 mt-config 与 mt-project 各一份私有复刻，等共享工具 crate 时合并。
- 各 crate 需要 `{app_data_dir}` 的（mt-ai 的 hook-server.json、mt-usage 的 usage.db）Wave 2 接线时统一走 `mt_config::app_data_dir()`。
- mt-project 的 `open_path_with_default_app` 改为直接 spawn `explorer.exe`（不再走 tauri-plugin-opener），含 `,`/前导 `-` 的路径需真机验证一次；不可靠则换 ShellExecuteW。
- **Wave 2 接线注意**：mt-project 的 git_pull/push、worktree 系列为阻塞调用，原靠 `#[tauri::command(async)]` 挪出主线程，现在必须由 mt-app 自己丢 background executor。

## 风险与决议记录

- **TerminalElement 是全项目最高风险件**：中英文混排逐列对齐 / IME / 选择剪贴板 / 拖拽 / 背景图五项验收，任意两条卡死即触发路线重估（gpui fork 或换路线），见方案文档第 6 节。
- Wave 1 期间 mt-pty 公开 API 冻结为「只增不改」，解除时间：Wave 1 全部验收后。
- 遗留知识入口：AI 状态判定三轮修复史与铁律（CLAUDE.md process_monitor 段）、v0.12.1 渲染对齐诊断手法（截图逐列测量，可复用于验收项 1）。

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
| D | mt-ai | hook 体系+状态判定+会话记录**逐字搬运**；StatusSink 注入；去重表与「降级结论落盘」铁律保留 | hook_server.rs、hook_registry.rs、process_monitor.rs、ai_sessions.rs、pty.rs 的 AI 识别段 | ✅ | 2026-08-18 主会话独立 target 复跑 181/181 绿；AiPerception 装配层出 observe_input/observe_output 两入口；PtyManager 的 AI 半边拆为 SessionTracker（8 张旁路表）；StatusEmitter 去重表与落盘铁律逐字保留；hook-server.json 端口文件格式不变 |
| E | mt-pty | conpty_bootstrap + pty.rs 存留部分；**公开 API 只增不改**；净删除三件套不搬 | pty.rs、conpty_bootstrap.rs | ✅ | 2026-08-18 主会话独立 target 复跑 59+1 doctest 全绿（含真起 cmd.exe 的端到端 6 条）；spawn 等原签名未动；退出监听改 try_wait 轮询（实测 Windows 下 reader EOF 路径等于本地 exit 不报退出）；autofill 抽成状态机且密码直写 writer 不过输入观察器 |
| F | mt-ui + mt-app | 自研 TerminalElement（逐 cell 绘制/宽字符对齐/默认背景不发 quad）+ 真实 PTY 端到端 demo | —（全新自研，替 xterm.js） | ✅ | 2026-08-18 主会话复核 8 测试绿+6s 启动冒烟通过（像素级人工验收见下方清单）：gpui Element 三段式实现；对齐方案=可合并 cell 拼 ShapedLine+不可合并（宽字符/回退/组合符）单独 shape 钉在 col×cell_width；事件驱动唤醒+16ms 节流；选择/滚动/256 色+truecolor/光标四态齐；实机 PostMessageW 注入按键跑通完整链路；**IME 未实现**（挂载点已留）、鼠标上报/damage 追踪未做 |

**并行纪律**（agent 提示词里已固化）：每个 agent 只写自己的 crate 目录；根 Cargo.toml 禁改；构建测试永远 `-p`；不自行 commit，由主会话验收后统一提交。

## Wave 2 —— 规划（派出时细化）

| 任务 | 依赖 | 说明 |
|---|---|---|
| mt-relay 搬运 🔵 | mt-ai（✅ 已落库） | 2026-08-18 提前派出（不等 Wave 1 收口）；mobile_relay.rs + mobile_mirror.rs；协议与 mobile/ 完全不动；桌面侧状态依赖全部抽注入 trait |
| mt-app 全壳 | Wave 1 全部 | store（对应 src/store.ts）、三栏 resizable、Tab 栏、SplitNode 树、文件树接 mt-project、状态灯接 mt-ai |
| 面板与 Modal | mt-app 全壳 | 终端配置 / AI 历史 / 用量统计 / 移动端面板 / 分支树 |
| i18n + 主题桥 | mt-app 全壳 | rust-i18n 字典从 src/locales/*.ts 转；theme_packs 配色映射 gpui-component 主题层 |

## TerminalElement 人工验收清单（等 F 验收提交后执行，复选框由验收人勾）

`$env:MT_UI_DEBUG_METRICS=1; cargo run -p mt-app`（该开关会打印字体度量，且字体被静默回退成非等宽时往 stderr 报警）

- [ ] **中英混排逐列对齐（最高优先）**：打多行 `你好abc世界XY`，与 xterm.js 版双开截图逐列量（复用 v0.12.1 手法）
- [ ] 颜色：16 色 + 256 色 + truecolor 色表脚本，bold/italic/underline/inverse 组合
- [ ] 光标：聚焦实心块+字反白，失焦空心框
- [ ] 滚轮方向：上滚见历史（若反了是 ScrollDelta.y 符号问题，一行取反）
- [ ] 选择：拖选松手自动复制、双击选词、三击选行、Ctrl+Shift+C/V
- [ ] resize：拖窗口边跑 vim 看重排
- [ ] alt screen：vim 里滚轮等价上下方向键
- [ ] IME 输中文：预期不工作，确认不崩即可（IME 是下一批交付项）

## 已验收提交

（首个 Wave 1 交付验收后开始记录：commit、crate、测试结果、与原实现的偏差摘要）

## 技术债与待修清单（迁移期产生）

- ~~mt-usage/ai_shim.rs~~ **已收编**（`826071a`）：删除临时副本，六处调用直连 mt-ai，grok_home 提为 pub。
- ~~ledger WAL BUSY 竞态~~ **已修复**（`f42ccce`）：open_raw 对该 pragma 按 5s 预算做 BUSY 限定重试；原单跑挂 5~8/10 轮的并发测试连跑 6 轮全绿。
- Cargo.lock 已把 rusqlite/libsqlite3-sys pin 到与 src-tauri 完全一致（0.40.1/0.38.1）。
- **SshConnection 归属决议**：mt-config 内复刻了 mt-core 的同形结构（serde 形状有回归测试钉住）。决议：config 是 sshConnections 的持久化归属方，其他 crate 统一引用 `mt_config::SshConnection`；mt-core 移入 crates/ 后改 re-export。
- `atomic_write` 在 mt-config 与 mt-project 各一份私有复刻，等共享工具 crate 时合并。
- 各 crate 需要 `{app_data_dir}` 的（mt-ai 的 hook-server.json、mt-usage 的 usage.db）Wave 2 接线时统一走 `mt_config::app_data_dir()`。
- mt-project 的 `open_path_with_default_app` 改为直接 spawn `explorer.exe`（不再走 tauri-plugin-opener），含 `,`/前导 `-` 的路径需真机验证一次；不可靠则换 ShellExecuteW。
- **Wave 2 接线注意**：mt-project 的 git_pull/push、worktree 系列为阻塞调用，原靠 `#[tauri::command(async)]` 挪出主线程，现在必须由 mt-app 自己丢 background executor。
- **mt-pty → src-tauri/mt-core 路径依赖**（parse_wsl_unc / scan_ssh_prompt / strip_ansi_codes 三个纯函数）：决议接受，不违反「mt-core 不提前物理移动」红线；收尾阶段 mt-core 移入 crates/ 后改成 workspace 依赖。
- mt-pty 退出监听为每会话一 watcher 线程轮询 try_wait（前 2s 每 50ms，此后 250ms）；pane 数量大时可换 WaitForSingleObject 单线程复用。
- 便携 ConPTY 资源目录暂按「与 exe 同目录」推断，GPUI 打包方案定型后复核。
- mt-ai ↔ mt-pty 接线口径（Wave 2）：输出活跃度靠 on_output tee；焦点序列常量已从 mt-pty 导出；「真实下发的 resize」用 resize_if_changed 返回值判定；observe_input 必须在字节交给 PTY **之前**调（焦点冷却先于 TUI 重绘，与原 write_pty 同序）。
- **mt-ai 同步化的两个慢函数**：get_ai_session_content / get_wsl_ai_sessions 原是 async command（WSL 9P+VM 冷启动秒级），现为同步函数，mt-app 接线时必须丢后台线程。
- mt-ai 也 vendored 了 parse_wsl_unc / strip_ansi_codes / atomic_write 三个纯函数（与 mt-pty 走 mt-core 路径依赖是两种解法），收尾统一去重。
- hook 二进制仍按「与主程序同目录 miniterm-hook(.exe)」定位；GPUI 壳产物布局定型后与 scripts/stage-sidecars.mjs 一起复查。

## 风险与决议记录

- **TerminalElement 是全项目最高风险件**：中英文混排逐列对齐 / IME / 选择剪贴板 / 拖拽 / 背景图五项验收，任意两条卡死即触发路线重估（gpui fork 或换路线），见方案文档第 6 节。
- Wave 1 期间 mt-pty 公开 API 冻结为「只增不改」，解除时间：Wave 1 全部验收后。
- 遗留知识入口：AI 状态判定三轮修复史与铁律（CLAUDE.md process_monitor 段）、v0.12.1 渲染对齐诊断手法（截图逐列测量，可复用于验收项 1）。

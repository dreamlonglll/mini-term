# GPUI 迁移进度看板

> 本文档只记「迁到哪了」，迁移方案与技术决策见 [gpui-migration.md](./gpui-migration.md)。
> 由主会话在每个任务派出 / 验收 / 提交节点更新；标注均为当地时间。
>
> 状态图例：⬜ 未开始 · 🔵 进行中（agent 已派出） · 🟡 已交付待验收 · ✅ 已验收提交 · ❌ 受阻（附原因）

## 总览

| 阶段 | 内容 | 状态 |
|---|---|---|
| 骨架 | 工作区 9 crate + 依赖选型 + 迁移映射（`aa9a7fc`） | ✅ 2026-08-18 |
| Wave 1 | 后端五块并行搬运 + TerminalElement 端到端 | ✅ 2026-08-18 全部验收入库（6/6） |
| Wave 2 | mt-relay、mt-app 全壳（store/三栏/Tab/分屏树） | ✅ 2026-08-18 两件均验收入库；面板/Modal/i18n/主题桥移入 Wave 3 |
| Wave 3 | G=mt-app UI 批（Modal/AI 历史+用量面板/通知/分屏比例+焦点导航）；H=mt-ui 渲染批（IME/鼠标上报/damage/主题桥）；I=mt-i18n 字典基建 | ✅ 2026-08-18 全部验收入库。G 经收尾 agent 补验：66 单测+4 集成全绿（老断言零改动），六模块齐（托盘明确未做），收尾另修 3 个真 bug（分屏比例恢复首帧 FALLBACK_AREA 基准错→改首帧量尺下帧铺树；窗口聚焦不清未读；折叠栏把 sizes 抹成最小值）+ 2 处资源问题（会话面板惰性加载防 WSL 冷启动、用量面板 Task 句柄无界增长）+6 单测；I ✅（`d2af55f`）；H ✅（`92390d4`） |
| Wave 4+ | 按 docs/gpui-parity-audit.md 30 条缺口逐批清零（第 0 层接线 → 基建 → 面板 → 整块新功能） | 🔵 2026-08-18 审计任务书落盘；J=mt-app 接线批 + K=mt-ui 视觉批已派出 |
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
| mt-relay 搬运 ✅ | mt-ai（✅ 已落库） | 2026-08-18 主会话复跑 32/32 绿；RelayHost/RelayEvents 两注入 trait（回调可能在 tokio 线程上来，实现方自己跳回 GPUI 主线程）；接线三硬要求：write_pty 必须全语义写穿口、start_session 后必回执 start_session_result、启动器命令文本绝不进快照（ADR 0002） |
| mt-app 全壳 ✅ | Wave 1 全部（✅） | 2026-08-18 主会话复跑 29/29 绿 + 隔离数据目录 8s 启动冒烟通过；9 模块 ~3.4k 行：tree.rs 纯数据层（17 测）/persist.rs 磁盘格式一字不改（7 测）/store.rs=AppStore Entity/pane/terminal_area/project_list/file_tree/ai 桥/ui 配色；实机三轮确认「恢复布局→hydrate→起 PTY」链路真跑通 |
| 面板与 Modal | mt-app 全壳 | 终端配置 / AI 历史 / 用量统计 / 移动端面板 / 分支树 |
| i18n + 主题桥 | mt-app 全壳 | rust-i18n 字典从 src/locales/*.ts 转；theme_packs 配色映射 gpui-component 主题层 |

## 中断现场（已解除）

2026-08-18 G 批中断现场经收尾 agent 处理完毕并验收入库，此段仅留档：当时六个新模块+七处修改未提交、未跑测试；收尾走了方式①（重派 agent），补验+修复+补测后由主会话复跑 66+4 全绿提交。

## Wave 4 起的任务书

**docs/gpui-parity-audit.md** 是 UI/UX 对照审计产出的 30 条缺口清单（分 4 层：接线型/基建型/面板补全/整块新功能），Wave 4 起每批从该清单挑条目，做完在清单上勾状态+注提交号。审计同时纠正了本看板 4 条旧记录：鼠标上报其实已接线；tab 拖拽排序/中键关闭 tab 原版本来就没有（撤销）；分屏比例跨重启问题已被 G 收尾修掉。

## Wave 3.5 接线清单（H/I 交付后累积，逐项做完勾掉）

- [ ] **TerminalPane 换用 TerminalView**：四步接线代码片段在 `crates/mt-ui/src/terminal/view.rs` 模块注释「宿主接线」段，照抄即可；**必须删掉**宿主的 track_focus/key_context/on_key_down/左键聚焦 on_mouse_down（IME 分流依赖按键放行，留着会双份处理且中文输入会漏 `n` 进 shell）
- [ ] 宿主在切 tab/关 pane 时调 `clear_preedit()`（防组合中失焦留预编辑残影）
- [ ] OSC 应答改 `mt_ui::terminal_color_rgb(&emulator, &theme, index)`，删 pane 里的 theme_color_rgb
- [ ] 主题切换入口接 `switch_to_theme_pack(&ThemePacks, id, window, cx)`；内置亮暗切换走 `switch_to_builtin`
- [ ] i18n：各 crate 挂 `mt-i18n.workspace = true`（先在根 Cargo.toml 的 workspace.dependencies 加 path 行——之前的 agent 都被禁改根文件）；启动时 `set_locale(cfg.locale)` + `add_locale_observer(|l| rust_i18n::set_locale(l.code()))` 桥接 gpui-component 内置组件；AppConfig 加 locale 字段；首启语言检测走 Win32 GetUserDefaultLocaleName → Locale::from_system_tag（Windows 上 LANG 环境变量通常不存在）
- [ ] 文案替换：TS 的 `t('ns.key')` → `t("ns", "key")` 或 `t_path("ns.key")`，key 一字未变可照 TSX 抄
- [ ] IME 人工验收 8 步（微软拼音组合/候选框跟随/方向键不漏/Esc 取消/失焦/emoji/英文直打回归）——H 报告原文已并入下方人工验收清单语境，跑 app 前必设 MT_APP_DATA_DIR

## Wave 3 拆法建议（mt-app 全壳 agent 留下的，已采信记档）

1. **Modal 批**（独立）：gpui_component::dialog + input → 终端配置/重命名/移除确认/添加项目；收编 pending_remove「点两次确认」临时方案
2. **分屏比例恢复 + 焦点导航**（小，独立）：ResizablePanel 喂像素初值或给 gpui-component 提百分比 API；focusAdjacentPane 几何最近邻
3. **通知/托盘批**（依赖 1）：unreadDonePaneIds / aiDoneOrder / 提示音 / 任务栏闪烁 / 托盘菜单；apply_ai_event 已留完成判定落点
4. **面板批**（独立，可与 1 并行）：AI 历史（mt_ai::sessions，两个慢函数必须丢后台）+ 用量统计（mt-usage）
5. **i18n + 主题桥**（独立）：字典从 src/locales/*.ts 转；ui.rs 常量表是唯一替换点；主题包接 mt_config::theme_packs → TerminalTheme + gpui-component 主题
6. 另有渲染侧缺口：IME（挂载点已留）、鼠标上报（MOUSE_MODE/SGR_MOUSE）、damage 追踪、下划线花样、split_states 塌陷不回收（极小泄漏）

## mt-app 全壳已知缺口（此段过时，以 docs/gpui-parity-audit.md 为准）

- ~~分屏比例跨重启回均分~~（G 收尾已修）；~~tab 重命名~~（G 已做）；~~tab 拖拽排序~~（原版也没有，撤销）；右键菜单/项目分组/AI 自动 resume/文件拖入终端 → 已并入审计清单
- 状态灯三形未复刻勾叉字形与旋转动画 → 审计清单 #9；中间栏折叠后仍渲染分隔条把手（gpui-component 行为）

## 开发纪律（跑 GPUI dev 实例）

- **必设 `MT_APP_DATA_DIR`** 指到隔离目录再 `cargo run -p mt-app`，否则会与装机版抢 `%APPDATA%\com.mini-term.app\`——2026-08-18 已发生一次：dev 实例 hook server 退到 23457 并覆盖 hook-server.json，装机版 hook 上报被指向死端口（已手工修复回 23456）。与 Tauri 侧 `--config` 覆盖 identifier 是同一目的。

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
- `is_wsl_unc_path`/parse_wsl_unc 判定已是工作区**第三份**复刻（mt-ai / mt-pty 走 mt-core / mt-relay），收尾统一去重。
- mt-relay 默认自持 2 线程 tokio 运行时（apply 惰性创建）；mt-app 若有全局运行时应改用 `with_runtime` 注入，避免进程双线程池。

## 风险与决议记录

- **允许第三方 GPUI UI 库**（2026-08-18 用户决议）：为达到与 Tauri 版类似的 UI/UX，允许引入第三方组件库——icon、table、tab、动画效果等均可用现成轮子，不必手搓。首选已在工作区的 `gpui-component`（Icon/lucide 图标、TabBar、Table、Modal、Dialog、Resizable、Switch、Tooltip、动画等）；它不够用时可再评估其他 crates.io 上的 gpui 生态库（注意必须兼容 `gpui 0.2.x`，避免依赖树出现两个 gpui）。新增 workspace 依赖需主会话在根 Cargo.toml 加行（子 agent 禁改根文件的纪律不变，需要时在报告里提出）。

- **TerminalElement 是全项目最高风险件**：中英文混排逐列对齐 / IME / 选择剪贴板 / 拖拽 / 背景图五项验收，任意两条卡死即触发路线重估（gpui fork 或换路线），见方案文档第 6 节。
- Wave 1 期间 mt-pty 公开 API 冻结为「只增不改」，解除时间：Wave 1 全部验收后。
- 遗留知识入口：AI 状态判定三轮修复史与铁律（CLAUDE.md process_monitor 段）、v0.12.1 渲染对齐诊断手法（截图逐列测量，可复用于验收项 1）。

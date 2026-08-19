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
| Wave 4+ | 按 docs/gpui-parity-audit.md 30 条缺口逐批清零（第 0 层接线 → 基建 → 面板 → 整块新功能） | 🔵 J ✅（`9246abf`）；K ✅（`b2fa0a0`）；L ✅（`04ee62b`）；M ✅（`14c84e9`，⚠️ gpui-component 无 svg 资产，图标一律走 mt-ui VectorIcon）；O ✅（`2bb0205`）；N ✅（`e91bb03`）；P ✅（`944baff`，主会话复跑 139+4 绿：搜索三连 #23/#24/#26 + overlay.rs 快捷键让路 + 三条快捷键；SearchModal 点结果暂走外部编辑器待 #29 回接）；Q ✅（主会话复跑 167+4 + mt-config 45+1 绿：#17 用量面板全套含 pricing.rs models.dev 拉取、#18 会话面板本体、右抽屉悬浮层化；BranchFamilyPanel 判归 fork 批；zed-reqwest 净新增 crate=0）；**batch-specs/ 已备齐 8 份规格**（设置/面板/移动端/GitUI/托盘/标题栏杂项/拖放分组列表/marker 文件预览），后续批次任务书直接引用；R ✅（`cb3282d`，主会话复跑 mt-app 188+4/mt-terminal 3/mt-i18n 12+3/mt-ui 129/mt-config 45+7+1 全绿）；V ✅（`a7e9e85` 合并 `9c70bde`，主会话复跑 227+4 全绿；Git 六组件+拓扑图+输出旁路，两入口转 Y 批）；S ✅（`75ef401` 合并 `ca74978`，主会话复跑 243+4 全绿；gpui 原生 hit-test，window_snap.rs 283 行不搬，Snap Layouts 免费）；U ✅（`d3ad441` 合并 `dccbc4f`，主会话复跑 265+4 + mt-relay 32 全绿；面板/二维码/RelayHost 接线，mt-relay 仅 +1 行纯 re-export）；T ✅（`gpui-batch-t` 分支提交合并 `8b1cc30`，主会话复跑 277+4 全绿；Win32 直写托盘+独立线程+RAII HICON，真机验证待收尾阶段）；W ✅（`c15d4e0` 合并 `dfe612d`，293+4 / mt-ui 131 全绿）；X ✅（合并 `266696e`，343+4 全绿）；Y ✅（`f609bfd` 快进合入，355+4 全绿；#12/#14/#15 清账，键盘导航/hover 缩略图记档）；Z ✅（合并 `2dbc52f`，387+4 / mt-ui 138 全绿；关窗确认/自建 toast/双音 WAV/长粘贴转文件/smartCopyPaste，#30 剩五小项转清尾批）；AA ✅（合并 `a2295a5`，400+4 / mt-ui 138 全绿；tree-sitter 高亮+CRLF 往返+两入口回接，Markdown 链接块待上游回调口）；fork ✅（合并 `c337716`，423+4 / mt-ai 181 / mt-ui 138 全绿；menu.rs 自定义元素子菜单+BranchFamilyPanel+pendingFork+lineage 自记账磁盘格式互读钉死）。**⏸ 2026-08-19 晚暂停点：用户指示 fork 批合并后停。待令批次=清尾批（版本红点/FirstRunGuide/dirKinds/pane 动画/三列表键盘导航/hover 缩略图/趋势图与过渡基件）→ 收尾-1（mt-core/mt-ssh 进 crates/+复刻去重）→ BB（SSH #28）→ 删 src-tauri 与 src、发版切换** | |
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

- [x] **TerminalPane 换用 TerminalView**（J 批）：track_focus/key_context/on_key_down/左键聚焦 on_mouse_down 已删干净；gpui 派发顺序（action 先于 key 监听）实证等价原版 capture-consume，写进 pane.rs 注释
- [x] 切 tab/关 pane 调 `clear_preedit()`（J 批：activate_pane 收上一焦点 pane + dispose_terminal 先收再 kill）
- [x] OSC 应答改 `mt_ui::terminal_color_rgb`（J 批，顺带消灭了旧 theme_color_rgb 把查背景答成前景的 bug 副本）
- [x] 主题切换入口（J 批：因 AppliedThemePack 缺语义色，绕开 switch_to_theme_pack 手拆四步；mt-ui 补 API 后可收回单函数——已转 K 批）
- [x] i18n 装配（L 批）：mt-app 挂 mt-i18n；启动 `i18n::install` 早于任何视图；⚠️ gpui-component 桥接**必须传 `Locale::bcp47()` 不是 `code()`**（其 ui.yml 键是 zh-CN，传 zh 静默回英文）且 install 时先手动桥一次（set_locale 只在变化时通知）；观察者只做进程级副作用，重绘走 `i18n::switch(locale, cx)`（观察者拿不到 &mut App）
- [x] 文案替换（L 批）：90 调用点 84 key；缺 key 的 7 条带 TODO(i18n) 待 TS 源头补后重生成字典（→ M 批）
- [ ] IME 人工验收 8 步（微软拼音组合/候选框跟随/方向键不漏/Esc 取消/失焦/emoji/英文直打回归）——用户已豁免 E2E，留给日后真机自验；跑 app 前必设 MT_APP_DATA_DIR

## Wave 4.5 接线清单（K 批 mt-ui 组件交付后累积，mt-app 消费批照抄）

1. `ui.rs::status_dot` → `StatusDot::new(("status", 稳定id), kind).size(px(11.)).color(status_color(s)).contrast(bg_elevated())`；⚠️ id 必须逐处唯一且跨帧稳定（with_animation 拿它当状态 key，重复会共享动画进度，随帧变会每帧从头转）；完整片段在 `icons/status.rs` 模块注释
2. `session_panel.rs` 的 "CX"/"GK"/"CL" 文本 → `BrandIcon::new(AiVendor::for_session(&s.session_type, s.model.as_deref())).size(px(13.))`
3. tab 栏 / pane 标题用 `AiVendor::from_session_type(&pane.agent)`（表达「跑的是哪个 CLI」；刻意不用 for_session 的模型优先口径）
4. 项目列表/文件树根 → `TechIcon::new(ProjectKind::from_str(..)?)`；文件树每行 → `FileIcon::new(&entry.name, entry.is_dir, expanded)`，git 状态着色走 `.color(..)`
5. 滚动条默认已开零改动生效；调样式才 `.scrollbar(ScrollbarStyle{..})`
6. 停留复制：`.selection_dwell(DwellConfig::from_secs(cfg.selection_auto_copy_secs))` + `.on_selection_copied(|_text, origin, _w, cx| 存 origin → 1s 后清)`；气泡 `CopiedTip::new(...)` 按**元素相对**坐标绝对定位；完整片段在 view.rs「后加的三件」
7. 背景图：根容器第一个 child 挂 `mt_ui::background_art(art)`（宿主从 `AppStore::background_art()` 取）；⚠️ 窗口级与逐终端**二选一**，同时开会画两遍、dim 平方
8. 主题包壳配色可退回 `switch_to_theme_pack` 单函数调用（AppliedThemePack 已带 colors + `color(ThemeSlot)`）；退皮肤直接 `switch_to_builtin`（已内含恢复内置主题），theme.rs 的四步绕路与 ThemeRegistry 绕路代码**可删**

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
- **J 批（122b5ca 后）记档**：`ThemePacks::open()` 钉死 `mt_config::paths::themes_dir()` 不认 `MT_APP_DATA_DIR`（mt-app 用 `ThemePacks::at()` 绕开，待 mt-config 统一）；`lookup_ai_session_cwd` 同步阻塞（仅存量无 cwd 会话触发）；resume 的会话 cwd 起 PTY 失败拿不到信号，以 `is_dir()` 预检代偿；`config.skin`（blueprint/fluent2）无对应色表未实现。
- **P 批记档（搜索三连 + 快捷键让路）**：
  - `overlay.rs` 是覆盖物栈的唯一真相（`thread_local`，不是 gpui `Global` —— `TerminalPane::drop` 要摘登记而那里拿不到 `cx`）。**Esc 只关最上层在 GPUI 里是结构性免费的**（按键沿焦点链派发），原版 `overlayStack` 那套栈顶判定只需保留「防叠开 + 快捷键让路」两件。
  - **让路两道闸**：① `Window::has_focused_input`（gpui-component 按 `Input` 的聚焦/失焦维护 `Root::focused_input`）等价原版 `isTypingTarget`；② `overlay::allows`。⚠️ 若哪天 `focused_input` 卡在 `Some`（输入框被聚焦着卸载且没触发 blur），**全部全局快捷键会一起哑** —— 点一下别处即恢复，排障先看这里。
  - **`Ctrl+F` 必须绑 action，不能绑 pane 容器的 `on_key_down`**：`TerminalView` 认得 Ctrl+F（`keystroke_to_bytes` → `\x06`）并 `stop_propagation`，而 key 监听从焦点节点往上冒泡、终端那层在容器之前。search_bar.rs 的模块注释已就地更正。
  - **`Input` 的 `up`/`down`/`enter` 是 action**，且单行模式下 `MoveUp`/`MoveDown` 直接 return 不 propagate → 外层容器的 `on_key_down` 收不到方向键。破法：谓词写 `"ProjectSwitcher > Input"`（`depth_of` 对 `Descendant` 返回最深层深度，与裸 `"Input"` 打平）+ 打平后按注册顺序**倒序**决胜负（壳的 `cx.bind_keys` 在 `gpui_component::init` 之后）。`enter` 不走这条：单行 `Enter` 处理器会 propagate 且无条件 `emit(PressEnter)`，订阅它更直白。
  - `Dialog` 当自绘浮层用的三件套：`.p_0()`（默认 24px 内边距会把自画的分隔线切断）、`.close_button(false)`（它画 `IconName::Close`，0.5.1 无 svg 资产 → 空白）、聚焦输入框要 `window.defer`（`open_dialog` 会在之后把焦点抢到面板上）。
  - `window.close_dialog` **不触发** `Dialog::on_close` → 程序化关闭必须走 `prompt::close_guarded`（自己摘覆盖物栈），否则该种类再也开不出来。
  - `mt_project::search::start_search` 自带专用后台线程，结果走 `futures::mpsc` 回主线程；**不要**塞进 `background_executor`（那是给会 await 的 future 用的，同步闭包会占死一根工作线程）。
  - `Palette` 补 `color_warning`（`--color-warning`；主题包按 `accentAlt` 映射，与 `themePackManager.ts` 同口径）。
  - 遗留：SearchModal 点结果依赖 `FileViewerModal`（#29）未迁移，现退到外部编辑器打开；结果列表无虚拟化；分组头无 sticky；`ProjectSwitcher` 面板高度按候选条数估算（`Dialog` 只吃固定高度，没有 `max-h` 语义）。
- **fork 批（c337716 后）记档**：家族面板每次悬停现拉无跨菜单缓存（会话极多时首展有后台扫描延迟，与原版同）；同 agent「全新会话误记成 fork 子会话」残余窗口仍在（磁盘边合并优先+首次身份即消费+pty 退出清登记三道压到最小，与原版同）；`AgentBranchCaps::resume_command` 是有意保留的无调用点代码（删了能力位表就残，一致性单测防漂）；menu.rs 自绘子菜单展开期间每次重绘调渲染闭包——有状态面板必须懒建缓存（view_branches_menu_item 是范例）；「带 cwd 失败重试」刻意不搬（GPUI spawn_pane 不因目录失败返 None）。
- **AA 批（a2295a5 后）记档**：Markdown 链接三条处置整块缺（gpui-component 0.5.1 链接写死 cx.open_url 无回调口——外链确认/锚点滚动/本地文件跳转与跳转历史栈 ← 都做不了，上游开口后可补）；HTML 只留源码态；avif 解不出兜底「默认工具打开」（gpui 的 image 默认 feature 无 avif）；混合行尾文件保存后统一为主行尾（刻意取舍）；2s 回声窗会吞真实外部修改（原版同款）；Cargo.lock 的 cc 被精确降到 1.2.67（tree-sitter-sequel 约束 ~1.2，rusqlite/libsqlite3-sys pin 未动已核对）——后续谁升 cc 会撞回来；svg 红蓝互换旧结论仅适用 Image::from_bytes 路，img(Resource::Path) 有 swap_rgba_pa_to_bgra 颜色正确（已修正 mt-ui 注释适用范围）。
- **Z 批（2dbc52f 后）记档**：⚠️ **本仓 HEAD 非 rustfmt-clean，任何批次禁跑 cargo fmt**（Z 批跑过一次全仓 57 文件重排，已逐字节回滚）；toast 点击必须 window.defer（嵌套 update panic 第二次现形，「toast 里触发 store 动作」都要 defer）；OnPaste 用返回值制（宿主回写会 panic，结构性堵死）；关 tab 确认框开着时按 ✕ 会被 open_guarded 静默挡下（自恢复）；SSH 粘贴不转存直粘原文（#28 接上补一支）；toast 超 5 条排队中定时器照跑（原版同款）；自定义提示音仍只认 .wav。
- **Y 批（f609bfd 后）记档**：三列表键盘导航整块未做（行级 track_focus 与全局 F2=RenamePane 绑定要同源判定，牵 hotkeys/main）；hover 250ms 缩略图未做（需 MiniTerminalElement 独立自绘件，#12/#15 共用）；行内重命名默认全选做不到（InputState::select_all 是 pub(super)，光标置尾近似）；FileTree 100ms 节拍任务常驻（无项目时只做一次原子读）；worktree 徽章只在路径集合变化与窗口重获焦点时刷（前台常驻时外部 remove 不被发现，与原版一致）；git_watch 多订阅者用读游标不用 dirty 位（共享窗口 A 清 B 漏刷），加消费方=加 variant，绝不另开旁路。
- **W 批（dfe612d 后）记档**：回滚缓冲装满后该 pane 的 marker 功能整体停摆（alacritty 无累计 evict 计数器，文本重定位补路已拍板不做）；刚饱和到下次 add/jump 之间 ⚑ 计数可能显示已废条数（原版同属性）；浮层是 TerminalArea 级单例（原版每 PaneGroup 一份，遮罩挡第二次点击退化为「先关再开」）；truncate_line 按字符切与原版 UTF-16 码元在 emoji 档差一位；「最后一条 marker 永远亮进行中圆点」是原版行为别当 bug；远程 pane 重连清 marker 归 #28。
- **X 批（266696e 后）记档**：Esc 取消内部拖拽未做（gpui 无内建，要在 Workspace 拦 escape 且与终端 Esc 透传打架）；起拖阈值 2px（gpui DRAG_THRESHOLD）vs 原版 5px 手感差异；外部拖入判定中一瞬按 valid 配色画；get_ordered_tree 线性查找 O(n²)（当前规模无感）；拖行尾 × 也会起拖（原版只豁免 input，无害）；⚠️ gpui on_drag_move 打给**所有**注册了该载荷类型的元素（无 hitbox 判定），命中闸必须走 dnd::hit_ratio——漏了整列亮指示线；原版 moveItem 目标组缺失丢整子树（UI 不可达）已兜底、ensureTree 铺 worktree 子项目自相矛盾未照抄。
- **T 批（8b1cc30 后）记档**：Win32 托盘层全部只经编译期校验（本批禁跑 app），真机首跑三查：图标能否出现、右键菜单 emoji 渲染、SM_CXSMICON 在 HiDPI 下是否偏小（偏糊则换 GetSystemMetricsForDpi 或固定 32px 让 shell 缩放，纯局部改）；进程被 process::exit 强杀时图标可能残留到用户悬停一次（Drop 覆盖正常关窗路径，未挂 on_app_quit）；非 Windows 无托盘（platform::start 恒 None，macOS NSStatusItem/Linux SNI 只换 platform 模块）。
- **U 批（dccbc4f 后）记档**：mt-relay +1 行纯 re-export（`StartSessionFailReason`，主会话确认接受——不加则宿主只能恒传 SpawnFailed 丢失败原因档位）；`write_pty` 回执语义弱化为「已排队写入」（预检到落地之间 pane 被关会静默丢，真出投诉再换 oneshot+2s 超时的路 B）；`RelayHost` 镜像有 ≤150ms 陈旧窗口（启动器保存后已额外立即刷一次）；退出时不停 mt-relay 自持 tokio 运行时（Runtime::drop 主线程收尾，理论多几十毫秒，要收紧得给 manager 加 shutdown()）；`can_start_session` 对 SSH 远程项目误判 true（ssh_connection_id 恒 None 如实读，mt-ssh 批接上自动生效）；面板正文高度定值 540px（gpui 无视口单位）；面板关闭无钩子（open_guarded 的 on_close 会覆盖 build 里同名回调），配对码到达走 overlay::contains + WeakEntity 双保险。
- **S 批（ca74978 后）记档**：**reduced-motion 未接**——原版通配规则会停掉 `.animate-blink`，用户机器正是 reduce → 装机版状态灯不闪、GPUI 版会闪；GPUI 无媒体查询等价物，需全局「减少动画」开关才能对齐（Win32 可用 SPI_GETCLIENTAREAANIMATION 探测，与 mt-ui spinner 同源问题）。关窗现走系统 WM_CLOSE → `on_window_should_close` **全仓未注册**：当前无 AI 会话确认（配置由 on_app_quit 的 save_config_now 兜住不丢），Z 批要同时改 `title_bar::request_close_window` + main.rs 注册回调。托盘消费口 `DoneScope::Unread`/`AiProjectKind::as_str` 带 dead_code 留 T 批。下拉开着时全窗遮罩让标题栏暂退 HTCLIENT（拖拽/Snap/三键失效，点一下恢复，与右键菜单同款）。未最大化时最上沿 ~SM_CYFRAME 像素判 HTTOP 点不到胶囊（gpui 内建，与原版取舍同源）。既有小瑕疵：`ui::with_alpha` doc 说乘性、实现是赋值（menu.rs 私有同名份同病），当前调用方底色全不透明无实害。
- **V 批（9c70bde 后）记档**：主题包无 diff 槽位——`Palette::from_pack` 按 success/error 派生 diff 四色，扩 `ThemeSlot` 会改主题包格式；拓扑图渐变是 8 段分段近似（gpui `paint_path` 单色，段间 2% 重叠防缝）；git_panel 中缝拖拽的 total 高度是推算（面板 bounds − 仓库栏 34 − 两 header 30），仓库栏高度变了要同步那个 `fixed` 常量；`git_watch` 是全局滚动窗口——不同 pane 输出共用一个 8KiB 窗口，理论上能拼出一次误命中（后果只是多刷一次）；`REPO_CACHE` 进程级 thread_local 不清理（与原版 Map 同形态）；worktree 弹窗 `create_error` 字段实际恒 None（Rust 无外层 catch，失败全进 `create_results`，字段留着对齐原版结构）；FileTree「查看变更」与项目列表「Worktrees」两入口转 Y 批（`open_file_diff` / `git_worktree::open(discover_repos=true)` 已就绪，只差菜单项+两条菜单序断言同步）。
- **R 批（cb3282d 后）记档**：UI 间距不随 uiFontSize 缩放（原版 Tailwind 的 rem 连内边距一起缩，GPUI 侧间距是像素字面量，10px/20px 极端档观感有差）；uiFontFamily 只取首个族名（gpui `font_family` 单值，整串仍原样落盘）；提示音自定义仅认 .wav——选择时非 wav 出警告条，但**已存的旧值不再提示**；skin（blueprint/fluent2）与终端连字 UI 置灰待底层能力；⚠️ USED_KEYS 大半 key 是动态传进 `t()` 的（section()/toggle_row()/MENU_GROUPS/hotkeys 表），文档注释那条 grep 抓不到，取全表必须连 settings.rs/hotkeys.rs 的 key 字面量一起扫（i18n.rs 表头已加警告）；`AppStore::background_art()` 的 dead_code 标注属误标（main.rs 实际在用）；深链 initial_page 已打通但两处入口都传 None（与原版一致）。
- **N 批（2bb0205 后）记档**：mt-project 无 reveal 语义（mt-app 自落 explorer `/select,` 走 raw_arg 防空格路径二次转义，建议上收 mt_project::editor）；`fs::delete_entry` 是硬删非回收站（文案「无法撤销」相符，后续可接 trash crate）；gpui-component `InputState::select_all` 是 pub(super)，prompt 默认值全选做不到；菜单键盘方向键导航/进场动画未做。

## Wave 5 批次排程（2026-08-19 主会话规划；当前为用户指示的暂停点，下午继续）

编排规矩（用户指令，已入长期记忆）：开发一律 Opus subagent；同时运行 subagent **≤3**；后台静默等通知、不主动读运行中 agent 输出；禁止 agent 套娃派子 agent；不做 E2E。节奏：交付 → 主会话独立复跑测试验收 → 提交 → 补位派下一批。跑 dev 实例给用户看效果用 `MT_APP_DATA_DIR=%LOCALAPPDATA%\mini-term-gpui-dev`。

| 批 | 内容（审计条目） | 任务书（docs/batch-specs/） | 派发前决策 / 前置 |
|---|---|---|---|
| R ✅（`cb3282d` 2026-08-19） | 设置面板 9 分页 + skin 色表（#19 + #5 剩余） | settings-pages.md | 已按决策落地：连字/皮肤渲染但置灰+说明词条；UI 字号字族 thread_local 快照真接上（84 处 text_size 换 ui::font_px）；about 页复用 zed-reqwest；原语全自绘；另收编键位表 hotkeys.rs 为唯一事实来源 |
| S ✅（`75ef401` 合并 `ca74978` 2026-08-19） | 自定义标题栏（#20） | titlebar-shell-misc.md §A | 已按决策落地：gpui 原生 WindowControlArea（源码核实直翻 HT* 系，Snap Layouts 免费，双击最大化落 DefWindowProc；Drag 区「正列」不挖洞——命中按 paint 序）；request_close_window 是 Z 批关窗确认唯一挂点（on_window_should_close 全仓仍未注册）；collect_ai_projects 已就位，DoneScope::Unread 留 T 批 |
| T ✅（合并 `8b1cc30` 2026-08-19） | 系统托盘（#21） | tray.md | 已按决策落地：独立 mt-tray 线程自建顶层隐藏窗口（不用 HWND_MESSAGE——收不到 TaskbarCreated）；HICON/HBITMAP 全 RAII；TrackPopupMenu 模态期 reentrancy 加固；推送收成 store 观察者一处+签名去重；⚠️ Win32 层仅编译期校验，真机三查（图标出现/emoji 菜单/HiDPI 尺寸）留收尾 |
| U ✅（`d3ad441` 合并 `dccbc4f` 2026-08-19） | 移动端中转（#22） | mobile-relay.md | 已按决策落地：qrcode 位矩阵自绘（码下附配对码文本，属新增信息面但不越 ADR 0002）；RelaySignal channel 泵回主线程（spawn_in）；write_pty 预检+乐观回执；发起会话多一道 PTY 存活预检（PTY 起不来时 pane 保留给用户看红字，与原版「建 pane 前 return」不同，属改良） |
| V ✅（`a7e9e85` 合并 `9c70bde` 2026-08-19） | Git 全套 UI（#27） | git-ui.md | 已按决策落地：输出旁路方案 (a)（reader 仅 AtomicBool 闸+8KiB 环形缓冲，GitPanel 可见期 100ms 节拍跑 7 字面量，Y 批扩多订阅者勿另开旁路）；uniform_list 虚拟化；三条动画无降级（cubic_bezier 自绘）；下拉走 menu.rs 以 ✓/● 字形代偿胶囊 |
| W | marker 体系（#25） | markers-fileviewer.md §A | 锚点漂移补路二选一（文本重定位 / 饱和剪枝）；alt screen 不打点是正确行为 |
| X | 拖放基建 + 项目分组（#8 + #13） | dnd-groups-lists.md §A/§B | gpui 内外拖同一套 on_drop API（原版两套 pointer 脚手架不搬）；on_drop 不带位置，before/inside/after 由 on_drag_move 存 view state |
| Y | 三列表收尾（#12/#14/#15/#9 剩余） | dnd-groups-lists.md §C/§D/§E | 重命名从 N 批弹窗改回行内编辑（顺带解 select_all 记档）；git 着色的 pty-output 触发必须 isAiPty 跳过 |
| Z | 壳层杂项 + Toast + 提示音（#30 + 细项） | titlebar-shell-misc.md §B/§C | 关窗确认=同步钩子返 false + 弹框 + force_close 标志再 remove_window；自建 toast.rs（gpui-component Notification 四条结构性缺口）；双音走内存合成 WAV + PlaySoundW(SND_MEMORY|SND_ASYNC)，Beep 会阻塞 UI 线程 |
| AA | 文件预览与编辑器（#29） | markers-fileviewer.md §B | CRLF 往返必实测；tree-sitter-languages feature 依赖决议（不开只有 JSON 一种语言，开了拖 30 个 cc crate，主会话拍板）；落地后回接 SearchModal 结果点击与文件树打开 |
| 收尾-1 | mt-core/mt-ssh 进 crates/ + 三方复刻去重 | 本文档技术债段 | BB 的前置；含 mt-sidecars path 依赖与 stage-sidecars.mjs 联动 |
| BB | SSH 全套 UI（#28） | 未提取（届时补规格） | 依赖收尾-1 |

另注：**fork 批**（BranchFamilyPanel + menu.rs 扩自定义元素子菜单 + pendingFork 体系 + tab/终端右键 fork 项 + session_lineage 写入端）不在上表，Q 批已把判断依据记进 session_panel.rs 模块注释，届时单独成批；趋势图 path chart 件与「一次性跑完自停」过渡动画基件为可选自绘基建，随需求批带走。

## 风险与决议记录

- **允许第三方 GPUI UI 库**（2026-08-18 用户决议）：为达到与 Tauri 版类似的 UI/UX，允许引入第三方组件库——icon、table、tab、动画效果等均可用现成轮子，不必手搓。首选已在工作区的 `gpui-component`（Icon/lucide 图标、TabBar、Table、Modal、Dialog、Resizable、Switch、Tooltip、动画等）；它不够用时可再评估其他 crates.io 上的 gpui 生态库（注意必须兼容 `gpui 0.2.x`，避免依赖树出现两个 gpui）。新增 workspace 依赖需主会话在根 Cargo.toml 加行（子 agent 禁改根文件的纪律不变，需要时在报告里提出）。

- **TerminalElement 是全项目最高风险件**：中英文混排逐列对齐 / IME / 选择剪贴板 / 拖拽 / 背景图五项验收，任意两条卡死即触发路线重估（gpui fork 或换路线），见方案文档第 6 节。
- Wave 1 期间 mt-pty 公开 API 冻结为「只增不改」，解除时间：Wave 1 全部验收后。
- 遗留知识入口：AI 状态判定三轮修复史与铁律（CLAUDE.md process_monitor 段）、v0.12.1 渲染对齐诊断手法（截图逐列测量，可复用于验收项 1）。

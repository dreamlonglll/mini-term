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
| Wave 4+ | 按 docs/gpui-parity-audit.md 30 条缺口逐批清零（第 0 层接线 → 基建 → 面板 → 整块新功能） | 🔵 J ✅（`9246abf`）；K ✅（`b2fa0a0`）；L ✅（`04ee62b`）；M ✅（`14c84e9`，⚠️ gpui-component 无 svg 资产，图标一律走 mt-ui VectorIcon）；O ✅（`2bb0205`）；N ✅（`e91bb03`）；P ✅（`944baff`，主会话复跑 139+4 绿：搜索三连 #23/#24/#26 + overlay.rs 快捷键让路 + 三条快捷键；SearchModal 点结果暂走外部编辑器待 #29 回接）；Q ✅（主会话复跑 167+4 + mt-config 45+1 绿：#17 用量面板全套含 pricing.rs models.dev 拉取、#18 会话面板本体、右抽屉悬浮层化；BranchFamilyPanel 判归 fork 批；zed-reqwest 净新增 crate=0）；**batch-specs/ 已备齐 8 份规格**（设置/面板/移动端/GitUI/托盘/标题栏杂项/拖放分组列表/marker 文件预览），后续批次任务书直接引用；R ✅（`cb3282d`，主会话复跑 mt-app 188+4/mt-terminal 3/mt-i18n 12+3/mt-ui 129/mt-config 45+7+1 全绿）；V ✅（`a7e9e85` 合并 `9c70bde`，主会话复跑 227+4 全绿；Git 六组件+拓扑图+输出旁路，两入口转 Y 批）；S ✅（`75ef401` 合并 `ca74978`，主会话复跑 243+4 全绿；gpui 原生 hit-test，window_snap.rs 283 行不搬，Snap Layouts 免费）；U ✅（`d3ad441` 合并 `dccbc4f`，主会话复跑 265+4 + mt-relay 32 全绿；面板/二维码/RelayHost 接线，mt-relay 仅 +1 行纯 re-export）；T ✅（`gpui-batch-t` 分支提交合并 `8b1cc30`，主会话复跑 277+4 全绿；Win32 直写托盘+独立线程+RAII HICON，真机验证待收尾阶段）；W ✅（`c15d4e0` 合并 `dfe612d`，293+4 / mt-ui 131 全绿）；X ✅（合并 `266696e`，343+4 全绿）；Y ✅（`f609bfd` 快进合入，355+4 全绿；#12/#14/#15 清账，键盘导航/hover 缩略图记档）；Z ✅（合并 `2dbc52f`，387+4 / mt-ui 138 全绿；关窗确认/自建 toast/双音 WAV/长粘贴转文件/smartCopyPaste，#30 剩五小项转清尾批）；AA ✅（合并 `a2295a5`，400+4 / mt-ui 138 全绿；tree-sitter 高亮+CRLF 往返+两入口回接，Markdown 链接块待上游回调口）；fork ✅（合并 `c337716`，423+4 / mt-ai 181 / mt-ui 138 全绿；menu.rs 自定义元素子菜单+BranchFamilyPanel+pendingFork+lineage 自记账磁盘格式互读钉死）。2026-08-19 夜恢复开工，清尾批拆三支并行（tail-shell/tail-lists/tail-anim）：**tail-shell ✅**（`5b98de1` 快进合入，430+4 全绿；启动版本自检+ActivityBar 更新按钮/FirstRunGuide/启动埋点/空态 shell 菜单；孤儿 PTY 评估不做；纠正审计两处口径——原版是独立更新按钮非红点、空态是左键菜单非右键）；**tail-anim ✅**（`50ad883` 合并 `a2705c7`，主会话复跑 439+4/mt-ui 165 全绿；reduce 闸+SPI 探测/一次性过渡基件/趋势图 chart 件/usage 淡入与排行条补间/pane 进场淡入/徽标闪烁，豁免面按 styles.css 逐条钉死）；**tail-lists ✅**（`38b23aa` 合并 `126cb3f`，主会话复跑 459+4/mt-ui 177 全绿；dirKinds 探测缓存/三列表键盘导航+F2 同源分流/MiniTerminalElement 悬停缩略图/行内重命名全选撤销 Y 批记档）；**收尾-1 ✅**（`92e6663` 合并，主会话复跑七 crate 全绿，mt-core 46/mt-ssh 26 入根工作区；git mv 保历史+五份复刻去重+SshConnection 方向反转为 mt-config re-export mt-core）；**polish ✅**（`f244fd8` 合并，主会话复跑 469+4/mt-ui 179 全绿；菜单键盘导航+menuPopIn/缩略图与 done-tag 进场/更新圆点闪烁接闸/prompt 与 tab 重命名全选/emptyHint 独立行——N 批两条遗留与 tail 三批的动效挂接点全部结账）；**BB-a ✅**（`6e03732` 合并，主会话复跑 553+4 全绿；remote_ssh 服务层/ssh_registry/ssh_conn 纯逻辑/store SSH 全套 action+start_pty SSH 分支，自持懒建 tokio，U/W/Z 三条 SSH 遗留结账）；**BB-b ✅**（`03d59db` 合并，主会话复跑 580+4/179 全绿；三 SSH 弹窗+env_vars 弹窗+全部入口面+远程链路接线+dead_code 清账，审计 #28 收官）。**2026-08-20 凌晨：审计 30 条全部 ✅ 或按决议记档，迁移开发面收官**；**fix1 ✅**（`6220b55` 合并；设置面板 min_w_0 修内容列被裁+对话框视口钳制+头部自绘、皮肤身份统一为目录名口径修应用失败，顺带清偿 J 批 ThemePacks::open 与 U 批面板定高两条记档）。**2026-08-20 凌晨终测：全工作区 26 个测试目标全绿（mt-app 583+4/mt-ai 181/mt-config 48/mt-core 46/mt-project 79/mt-pty 59/mt-relay 32/mt-ssh 26/mt-ui 180/mt-usage 58/mt-i18n 19/mt-terminal 3），本轮夜间批次收官**。E2E 已禁（用户指示），删 src-tauri 与 src、发版切换留用户拍板 | |
| 收尾 | mt-ssh/mt-core 移入 crates/（✅ 收尾-1，`92e6663`）、删 src-tauri/ 与 src/、发版切换 | 🔵 |

## Wave 1 —— 2026-08-18 派出 6 个并行 agent

| # | crate | 任务 | 来源（src-tauri/src/） | 状态 | 验收记录 |
|---|---|---|---|---|---|
| A | mt-usage | 用量统计整块去 Tauri 化 | usage_stats/{mod,turns,ledger,aggregate,pricing}.rs（~3.5k 行） | ✅ | 2026-08-18 主会话独立 target 复跑 58/58 绿（首轮挂的正是已知 flaky 并发测试，重跑即过）；turns/aggregate/pricing 仅 5 处路径/可见性差异；async 查询改同步、emit 改 SyncSink；临时 `ai_shim.rs` 待收编 |
| B | mt-config | 配置持久化 + 主题包；app_data_dir 改 dirs 拼接，保留 migrate_legacy_app_data | config.rs、theme_packs.rs | ✅ | 2026-08-18 主会话独立 target 复跑 41+1 doctest 全绿；已查证 dirs::data_dir()+identifier 与 Tauri v2 同磁盘位置（Roaming）并有测试钉住；ConfigToken→ConfigStore 字段、read_theme_asset 改返回 Vec<u8>；SshConnection 本地复刻防 mt-core 耦合 |
| C | mt-project | fs/git/search/editor/wsl_distros；emit 改注入回调；**不搬 remote_ssh** | fs.rs、git.rs、search.rs、editor.rs、wsl_distros.rs | ✅ | 2026-08-18 主会话独立 target 复跑 76/76 绿；FsWatcher 注入 sink、search_id 消失改 SearchHandle、editor 拆纯函数不依赖 mt-config、opener 改平台原生 spawn；顺修 get_worktree_branches 的 UNC 判断隐性 bug；⚠️ git 阻塞调用需调用方自己丢后台执行器 |
| D | mt-ai | hook 体系+状态判定+会话记录**逐字搬运**；StatusSink 注入；去重表与「降级结论落盘」铁律保留 | hook_server.rs、hook_registry.rs、process_monitor.rs、ai_sessions.rs、pty.rs 的 AI 识别段 | ✅ | 2026-08-18 主会话独立 target 复跑 181/181 绿；AiPerception 装配层出 observe_input/observe_output 两入口；PtyManager 的 AI 半边拆为 SessionTracker（8 张旁路表）；StatusEmitter 去重表与落盘铁律逐字保留；hook-server.json 端口文件格式不变；⚠️ 2026-09-02 追记：本批只核了模块内容、漏搬了 `lib.rs::setup` 里两句启动期自愈的**调用点**（详见下方「Wave 1 D 批的漏搬追记」），`71eee1e` 已补 |
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

- ~~剪贴板图片粘贴整块缺失~~ **已补回**（2026-08-20）：Z 批只搬了 audit #30 的长文本那一半，图片这一半连同 `Alt+V` 兜底一并没搬（`clipboard.rs` 模块头当时明写「本批的一处收窄」）。**症状是按 `Ctrl+V` 毫无反应**——gpui 的 Windows 剪贴板只认 `PNG`/`JFIF`/`GIF`/`image/svg+xml` 四个**注册格式**（`platform/windows/clipboard.rs` 的 `FORMATS_SET`），截图工具放的是 `CF_DIB`，于是 `read_from_clipboard()` 连 `None` 都不给，`resolve_paste` 直接静默返回。修法是把装机版 `src-tauri/src/clipboard.rs` 的 `win` 模块整体搬进 `mt-app/src/clipboard.rs`（含 `1fcf1bc` 那轮 `parse_dib` 越界读/整数溢出加固与三个回归测试），另补三处：① `read_bitmap` 的缓冲区尺寸按同口径加固（装机版漏的裸 `width*height*4`，理论上能溢出成小缓冲）；② 读取结果分**「没有图」/「有图但读不出」**两态——后者才退 `Alt+V`，合并成 `Option` 的话 `BI_BITFIELDS` 压缩位图会被当成「剪贴板是空的」，又回到静默；③ 非 Windows 走 gpui 的图片 entry（原始编码字节原样写盘，装机版本就只有 Windows）。远程 pane 复用 BB-a 的 SFTP 通道（`RemotePaste::File`），断链时**只提示不粘**（图片没有原文可退，`Alt+V` 对远端 agent 无效——它读的是远端剪贴板）。⚠️ Win32 直读路径只经编译期校验与 `parse_dib` 单测，真机三查（截图工具粘出路径、图文混排按图片处理、`BI_BITFIELDS` 退 Alt+V）待做。
- ~~终端最小对比度整块缺失~~ **已补回**（2026-08-20）：装机版靠 xterm.js 的一行 `minimumContrastRatio: 4.5`（`0e1fea8`，注释点名就是 **Claude Code 的 AskUserQuestion 提问行**——近黑前景配默认暗背景，不选中看不见），随 `b52a654` 删 `src/` 一并消失，GPUI 版 `terminal/colors.rs` 是纯映射层、**全仓无任何对比度兜底**（grep `luminance|contrast_ratio|wcag` 零命中）。等价物落在 `colors.rs::ensure_contrast`（WCAG 相对亮度 + 按通道 10% 等比推进，逐字对齐 xterm.js 的 `ensureContrastRatio`，不转 HSL）。三处非显然点：① **必须双向试**——单向「暗前景就压得更暗」在中间调背景上会失效（灰度 < 116 的底上压到纯黑也只有 3.9x，到这里放弃 = 等于没修），所以远离方向推不到就换方向重推、取更优的一侧；② **插入点夹在 INVERSE 与 HIDDEN 之间**（`element.rs`）——在 INVERSE 后才对得上真正画出来的那一对，在 HIDDEN 前才不会把 `read -s` 的密码强行显形；③ **性能靠两条对策叠加**：取色在「每帧遍历全部可见格子」的循环里、行缓存只缓存 shaping，裸算是 6 万次 `powf`/帧，故先用 `has_visible_ink` 滤掉空格（一屏大头）再走 8 槽轮转的 `ContrastMemo`。缩略图（`mini.rs`）同步接入，参照色恒为 `theme.background`（它整块底就是这个色）。已知不准两条**与装机版同口径**：背景图皮肤下 `TerminalTheme::background` 是半透明的、真正在后面的氛围图亮度不可知（只看 RGB 忽略 alpha，算出来是名义对比度）；选中/查找命中的半透明高亮画在文字之下、不进参照。阈值硬编码 4.5 沿用装机版基线，真要可配落点是 `TerminalTheme` 加字段（0 或 1.0 = 关闭）而不是加用户开关。⚠️ 副作用是刻意的低对比文字会一并被提亮（vim 注释、`git log --graph` 装饰线、diff 上下文行），装机版带着 4.5 跑了一个多月无投诉，属已验证可接受的取舍；真机验收待做。
- ~~mt-usage/ai_shim.rs~~ **已收编**（`826071a`）：删除临时副本，六处调用直连 mt-ai，grok_home 提为 pub。
- ~~ledger WAL BUSY 竞态~~ **已修复**（`f42ccce`）：open_raw 对该 pragma 按 5s 预算做 BUSY 限定重试；原单跑挂 5~8/10 轮的并发测试连跑 6 轮全绿。
- Cargo.lock 已把 rusqlite/libsqlite3-sys pin 到与 src-tauri 完全一致（0.40.1/0.38.1）。
- ~~SshConnection 归属决议~~ **已落地（收尾-1，方向反转）**：`mt-config` re-export `mt-core` 的定义（非原记档的 mt-core re-export mt-config——那会把 zip/sha2/anyhow 拖进三个 sidecar 小二进制且 config_reader 与 ConfigStore 重影）；名字仍是 `mt_config::SshConnection`，serde 形状测试改钉共享定义，论证在 config.rs/mt-core lib.rs/根 Cargo.toml 三处互指。
- ~~`atomic_write` 三份复刻~~ **已去重（收尾-1）**：唯一实现在 `mt-core/src/atomic_file.rs`，mt-ai/mt-config/mt-project 改 re-export；⚠️ src-tauri/src/fs.rs 还有第 4 份，刻意不动（老应用整体删除时随之消失）。
- 各 crate 需要 `{app_data_dir}` 的（mt-ai 的 hook-server.json、mt-usage 的 usage.db）Wave 2 接线时统一走 `mt_config::app_data_dir()`。
- mt-project 的 `open_path_with_default_app` 改为直接 spawn `explorer.exe`（不再走 tauri-plugin-opener），含 `,`/前导 `-` 的路径需真机验证一次；不可靠则换 ShellExecuteW。
- **Wave 2 接线注意**：mt-project 的 git_pull/push、worktree 系列为阻塞调用，原靠 `#[tauri::command(async)]` 挪出主线程，现在必须由 mt-app 自己丢 background executor。
- ~~mt-pty → src-tauri/mt-core 路径依赖~~ **已收编（收尾-1）**：mt-core 已入 crates/，mt-pty 改 `mt-core.workspace = true`，代码零改动。
- mt-pty 退出监听为每会话一 watcher 线程轮询 try_wait（前 2s 每 50ms，此后 250ms）；pane 数量大时可换 WaitForSingleObject 单线程复用。
- 便携 ConPTY 资源目录暂按「与 exe 同目录」推断，GPUI 打包方案定型后复核。
- mt-ai ↔ mt-pty 接线口径（Wave 2）：输出活跃度靠 on_output tee；焦点序列常量已从 mt-pty 导出；「真实下发的 resize」用 resize_if_changed 返回值判定；observe_input 必须在字节交给 PTY **之前**调（焦点冷却先于 TUI 重绘，与原 write_pty 同序）。
- **mt-ai 同步化的两个慢函数**：get_ai_session_content / get_wsl_ai_sessions 原是 async command（WSL 9P+VM 冷启动秒级），现为同步函数，mt-app 接线时必须丢后台线程。
- ~~mt-ai vendored 三纯函数~~ **已去重（收尾-1）**：全部改走 mt-core；`mt_ai::util` 不再导出 `WslPath`（私有模块无 API 影响，要写类型名直接用 `mt_core::WslPath`）。
- hook 二进制仍按「与主程序同目录 miniterm-hook(.exe)」定位；GPUI 壳产物布局定型后与 scripts/stage-sidecars.mjs 一起复查。
- ~~is_wsl_unc_path 第三份复刻~~ **已去重（收尾-1）**：mt-relay 判定体改调 `mt_core::parse_wsl_unc(path).is_some()`，函数与测试原样保留。
- mt-relay 默认自持 2 线程 tokio 运行时（apply 惰性创建）；mt-app 若有全局运行时应改用 `with_runtime` 注入，避免进程双线程池。
- **J 批（122b5ca 后）记档**：~~ThemePacks::open() 不认 MT_APP_DATA_DIR~~（fix1 已统一：mt-config 新增 active_data_dir，theme.rs 绕路结清）；`lookup_ai_session_cwd` 同步阻塞（仅存量无 cwd 会话触发）；resume 的会话 cwd 起 PTY 失败拿不到信号，以 `is_dir()` 预检代偿；~~`config.skin`（blueprint/fluent2）无对应色表未实现~~ **已结清为「不做」（2026-08-25，用户指示）**：设置里的「皮肤」单选段整段移除，`AppConfig::skin` 字段与五条 `settings.appearance.skin*` 词条一并删除（存量键靠 serde 忽略 + db 侧 stale key 清理自然消散）。皮肤自此只有两档——默认（主题段 dark/light/auto）与外置主题包。
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
- **fix1 批（6220b55 后）记档**：设置面板根因=flex 子列 min-width:auto 兜底 min-content 而 gpui 量 min-content 不给换行宽（gpui text.rs:347），**任何 flex_1+overflow_y_scroll 的内容列都要补 min_w_0**——同病残留清单：modal/prompt/git_worktree/project_switcher 共 9 处定值宽弹窗未套 clamp helper（仅 git_worktree 600px 有小屏风险，每处一行）；皮肤身份口径=**目录名**（原版 themePackManager「以目录名为准」），卡片副标题随之显示目录名，mt-ui `list_theme_packs` 返回类型改 `ThemePackListing`、`AppliedThemePack.theme_id` 语义改目录名；**MT_APP_DATA_DIR 隔离边界**：config.json+themes/ 隔离，hook-server.json 刻意不隔离（否则 dev 实例收不到 hook）；皮肤卡片原版 grid-cols-2 两列 GPUI 恒一列（改 flex_1+min_w(220) 可对齐，未动）；真机四查点：窄窗设置面板不裁/标题分隔线与 ✕/ember-new 应用成功含背景图/移动端面板矮窗不出界。
- **BB-b 批（03d59db 后）记档**：分组下拉用「▾ 弹 menu.rs + 手输」替代原版 GroupCombobox（gpui 无失焦-抢点等价物）；拖拽走 gpui 原生 on_drag/on_drop 不搬 mousedown 脚手架，落点高亮虚线边框替 outline（恒占位防抖）；弹窗顶栏自绘三件套（p_0/close_button(false)/panel_header——Dialog 默认内边距切断满幅中缝）；会话面板断链多一句提示属刻意改良（原版吞成空表看不出断链）；远程项目无 fs watcher（手动刷新）/无单链压缩/会话正文一次性全量读，均与原版一致；ActivityBar 按钮序纠正为 用量→设置→SSH→移动端（U 批注释写反了原版位置）；remote_project 回车提交经「订阅置标志→builder defer」两跳（订阅拿不到 &mut Window）；真机首验三点：拖连接入组高亮/断线遮罩点重连/SSH 子菜单分组标题。
- **BB-a 批（6e03732 后）记档**：SSH autofill 在 spawn 之后 arm（装机版在之前；窗口微秒级而 ssh 提示要等 TCP+KEX 几十毫秒够不着；彻底消除需 mt_pty::PtyOptions 加 autofill 字段）；`toml_edit` 在 mt-app 直接钉 0.22（跟随 mt-ai/mt-usage 先例未入 workspace.dependencies，三处待统一）；tokio 显式补 net feature（enable_all 只在开 net 时才 enable_io，不该依赖 russh 的声明捎带）；`ssh_registry::ssh_cli_binary_path` 是 GPUI dev 产物布局的又一受害者（dev 下 SKILL.md 指向不存在的 mt-ssh-cli 路径，装机版正确，与第 117/121 条合并处理）；remote_ssh 的 tokio runtime 刻意不随退出 shutdown（与 mt-relay 同决策）；BB-b 落地时删 remote_ssh/ssh_conn/ssh_registry 顶部与 store 的共 5 处 allow(dead_code)。
- **polish 批（f244fd8 后）记档**：菜单选中项是视图状态非 DOM 焦点（「关闭还焦点」纪律的前提），高亮与 hover 同底色（原版两者共用同一条 CSS）；menuPopIn/pane 进场均不做 scale（gpui 无 transform，改尺寸会挪文字/触发字号反解），只淡入+位移；浮层进场期间 6px 负 margin 不参与 anchored 贴边测量，贴窗缘弹出前 160ms 可能 ≤6px 越界、落位即正（不修——要动 gpui 测量口径）；done-tag 过冲缩放只动水平内边距防行高抖；`modal::open_rename_pane` 顺带修了默认值全选（原版同走 showPrompt）；真机验收点：用户机器 reduce=on，更新圆点与 done-tag 应静止/瞬时，验动效需临时开系统「动画效果」。
- **tail-anim 批（50ad883 后）记档**：S 批「reduced-motion 未接」已清偿——`mt_ui::motion` 进程级闸 + mt-app `SPI_GETCLIENTAREAANIMATION` 探测（窗口重激活变了才刷新），豁免面逐条对照 styles.css reduce 段并有常量测试钉死（pane-enter/usage-fade/rank-bar 点名豁免照播，blink/pulse/toast 过闸停，spinner 只放慢 2.4s 不停——停住的 spinner 像卡死）；pane 进场只淡入不缩放（gpui 无 transform，改尺寸会触发 PTY resize 链）；`pane_enter` 表与 split_states 同档不按帧回收；趋势图面积 path 每帧重建是 gpui 无保留场景的结构性代价（模型已缓存+共线顶点已压）；面积渐变 16 段**严格相邻不重叠**（与 V 批 2% 重叠相反——半透明填充重叠会二次混合出深线）；`cubic_bezier` 在 mt-ui/mt-app 各一份（合并要动 ui.rs 公开签名，留债）；`.done-tag` 的 tagFadeIn 两侧都缺（→ polish 批）；Win32 探测层只经编译期校验，真机验收点=reduce 下状态灯慢转不闪/toast 直现/浮层与 pane 淡入照播。
- **tail-lists 批（38b23aa 后）记档**：**焦点环缺位需全局方案**——原版靠 CSS `:focus-visible` outline，gpui 无 outline（border 挤布局）也无 focus-visible 语义（鼠标点击同样触发 `.focus()` 样式），三列表键盘焦点当前不可见，候选=focus+hover 抑制启发式/内阴影/接受有差；**点列表行会把焦点从终端收走**（对齐原版 tabIndex=0 语义，Delete/F2 可达的前提，眼验点）；三列表行 tab_index(0) 未做 tab_group 隔离（Tab 序退化为树序，观感不对时给列表容器加 `.tab_group().tab_stop(false)`）；dirKinds 失效只认项目根（照抄原版，子工程 Cargo.toml 变动不失效）；`exited_ptys` 已补但只接缩略图断开遮罩，重连覆盖层/resetPaneForReconnect 归 #28；缩略图字号 14px 上限钳住小 grid（原版 cover 无上限糊成色块，刻意）；缩略图跳过 SGR8 隐藏格（刻意加严防 `read -s` 泄露）；`InputState::select_all` 记档撤销——公开 action `SelectAll` 经 on_next_frame 后 dispatch_action 即等价 Ctrl+A（prompt.rs 默认值全选可用同招 → polish 批）。
- **收尾-1 批（92e6663 后）记档**：mt-core/mt-ssh 的 edition/version 保持**显式**值（旧 Tauri 侧仍编译它们，勿改 workspace 继承——两 Cargo.toml 头部已标死）；GPUI dev 产物布局问题仍开着：stage-sidecars 的 DEV_EXE_DIR 是 src-tauri/target/debug 而 mt-app.exe 在根 target/debug，GPUI dev 下 hook 二进制/portable-conpty 不在 exe 旁（ConPTY 静默回落系统版、hook 注册指向不存在文件）——留打包批与既有第 117/121 条合并处理；release.yml 的 rust-cache 不再把 mt-core/mt-ssh 当 local package 清理，缓存略增无正确性影响；fresh worktree 跑 `cd src-tauri && cargo check` 会撞 externalBin/resource 产物门（binaries/*.exe 与 resources/portable-conpty 是 gitignored，需先 stage）。
- **tail-shell 批（5b98de1 后）记档**：更新按钮圆点是静态的，闪烁挂接点在 `activity_bar::update_button` 注释里标死（等 tail-anim 全局 reduce 闸；用户机器 reduce 下装机版本来就不闪）；FirstRunGuide 缺 SSH 远程入口（词条 `app.firstRun.addRemote` 已在字典，#28 落地补第二颗按钮，审计记 🟡）；「添加本地项目」走 open_add_project 弹窗非系统目录选择框（与项目列表入口同一条路，UNC/WSL 可手输）；startup_trace 无条件 eprintln 与装机版一致（如加 MT_STARTUP_TRACE 门控属主动增强需拍板）；孤儿 PTY 评估不做（单进程无失引用链路，异常退出内核收句柄，脱控制台的孙进程两侧同款管不到）；空态 emptyHint 独立提示行是既有偏差未顺手改（词条在字典）。
- **AA 批（a2295a5 后）记档**：Markdown 链接三条处置整块缺（gpui-component 0.5.1 链接写死 cx.open_url 无回调口——外链确认/锚点滚动/本地文件跳转与跳转历史栈 ← 都做不了，上游开口后可补）；HTML 只留源码态；avif 解不出兜底「默认工具打开」（gpui 的 image 默认 feature 无 avif）；混合行尾文件保存后统一为主行尾（刻意取舍）；2s 回声窗会吞真实外部修改（原版同款）；Cargo.lock 的 cc 被精确降到 1.2.67（tree-sitter-sequel 约束 ~1.2，rusqlite/libsqlite3-sys pin 未动已核对）——后续谁升 cc 会撞回来；svg 红蓝互换旧结论仅适用 Image::from_bytes 路，img(Resource::Path) 有 swap_rgba_pa_to_bgra 颜色正确（已修正 mt-ui 注释适用范围）。
- **Z 批（2dbc52f 后）记档**：⚠️ **本仓 HEAD 非 rustfmt-clean，任何批次禁跑 cargo fmt**（Z 批跑过一次全仓 57 文件重排，已逐字节回滚）；toast 点击必须 window.defer（嵌套 update panic 第二次现形，「toast 里触发 store 动作」都要 defer）；OnPaste 用返回值制（宿主回写会 panic，结构性堵死）；关 tab 确认框开着时按 ✕ 会被 open_guarded 静默挡下（自恢复）；~~SSH 粘贴不转存直粘原文~~（BB-a 已结账：SSH 分支接 upload_paste，断链时仍粘原文）；toast 超 5 条排队中定时器照跑（原版同款）；自定义提示音仍只认 .wav。
- **Y 批（f609bfd 后）记档**：三列表键盘导航整块未做（行级 track_focus 与全局 F2=RenamePane 绑定要同源判定，牵 hotkeys/main）；hover 250ms 缩略图未做（需 MiniTerminalElement 独立自绘件，#12/#15 共用）；行内重命名默认全选做不到（InputState::select_all 是 pub(super)，光标置尾近似）；FileTree 100ms 节拍任务常驻（无项目时只做一次原子读）；worktree 徽章只在路径集合变化与窗口重获焦点时刷（前台常驻时外部 remove 不被发现，与原版一致）；git_watch 多订阅者用读游标不用 dirty 位（共享窗口 A 清 B 漏刷），加消费方=加 variant，绝不另开旁路。
- **W 批（dfe612d 后）记档**：回滚缓冲装满后该 pane 的 marker 功能整体停摆（alacritty 无累计 evict 计数器，文本重定位补路已拍板不做）；刚饱和到下次 add/jump 之间 ⚑ 计数可能显示已废条数（原版同属性）；浮层是 TerminalArea 级单例（原版每 PaneGroup 一份，遮罩挡第二次点击退化为「先关再开」）；truncate_line 按字符切与原版 UTF-16 码元在 emoji 档差一位；「最后一条 marker 永远亮进行中圆点」是原版行为别当 bug；~~远程 pane 重连清 marker~~（BB-a 已结账：reset_pane_for_reconnect → dispose_terminal 一并回收 marker/游标/退出登记/pendingFork）。
- **X 批（266696e 后）记档**：Esc 取消内部拖拽未做（gpui 无内建，要在 Workspace 拦 escape 且与终端 Esc 透传打架）；起拖阈值 2px（gpui DRAG_THRESHOLD）vs 原版 5px 手感差异；外部拖入判定中一瞬按 valid 配色画；get_ordered_tree 线性查找 O(n²)（当前规模无感）；拖行尾 × 也会起拖（原版只豁免 input，无害）；⚠️ gpui on_drag_move 打给**所有**注册了该载荷类型的元素（无 hitbox 判定），命中闸必须走 dnd::hit_ratio——漏了整列亮指示线；原版 moveItem 目标组缺失丢整子树（UI 不可达）已兜底、ensureTree 铺 worktree 子项目自相矛盾未照抄。
- **T 批（8b1cc30 后）记档**：Win32 托盘层全部只经编译期校验（本批禁跑 app），真机首跑三查：图标能否出现、右键菜单 emoji 渲染、SM_CXSMICON 在 HiDPI 下是否偏小（偏糊则换 GetSystemMetricsForDpi 或固定 32px 让 shell 缩放，纯局部改）；进程被 process::exit 强杀时图标可能残留到用户悬停一次（Drop 覆盖正常关窗路径，未挂 on_app_quit）；非 Windows 无托盘（platform::start 恒 None，macOS NSStatusItem/Linux SNI 只换 platform 模块）。
- **U 批（dccbc4f 后）记档**：mt-relay +1 行纯 re-export（`StartSessionFailReason`，主会话确认接受——不加则宿主只能恒传 SpawnFailed 丢失败原因档位）；`write_pty` 回执语义弱化为「已排队写入」（预检到落地之间 pane 被关会静默丢，真出投诉再换 oneshot+2s 超时的路 B）；`RelayHost` 镜像有 ≤150ms 陈旧窗口（启动器保存后已额外立即刷一次）；退出时不停 mt-relay 自持 tokio 运行时（Runtime::drop 主线程收尾，理论多几十毫秒，要收紧得给 manager 加 shutdown()）；~~can_start_session 对 SSH 远程项目误判~~（BB-a 已结账：接上「添加远程项目」入口自动生效，to_relay_project 抽出并有测试钉死）；~~面板正文高度定值 540px~~（fix1 已改按 76vh 现算走 clamp helper）；面板关闭无钩子（open_guarded 的 on_close 会覆盖 build 里同名回调），配对码到达走 overlay::contains + WeakEntity 双保险。
- **S 批（ca74978 后）记档**：**reduced-motion 未接**——原版通配规则会停掉 `.animate-blink`，用户机器正是 reduce → 装机版状态灯不闪、GPUI 版会闪；GPUI 无媒体查询等价物，需全局「减少动画」开关才能对齐（Win32 可用 SPI_GETCLIENTAREAANIMATION 探测，与 mt-ui spinner 同源问题）。关窗现走系统 WM_CLOSE → `on_window_should_close` **全仓未注册**：当前无 AI 会话确认（配置由 on_app_quit 的 save_config_now 兜住不丢），Z 批要同时改 `title_bar::request_close_window` + main.rs 注册回调。托盘消费口 `DoneScope::Unread`/`AiProjectKind::as_str` 带 dead_code 留 T 批。下拉开着时全窗遮罩让标题栏暂退 HTCLIENT（拖拽/Snap/三键失效，点一下恢复，与右键菜单同款）。未最大化时最上沿 ~SM_CYFRAME 像素判 HTTOP 点不到胶囊（gpui 内建，与原版取舍同源）。既有小瑕疵：`ui::with_alpha` doc 说乘性、实现是赋值（menu.rs 私有同名份同病），当前调用方底色全不透明无实害。
- **V 批（9c70bde 后）记档**：主题包无 diff 槽位——`Palette::from_pack` 按 success/error 派生 diff 四色，扩 `ThemeSlot` 会改主题包格式；拓扑图渐变是 8 段分段近似（gpui `paint_path` 单色，段间 2% 重叠防缝）；git_panel 中缝拖拽的 total 高度是推算（面板 bounds − 仓库栏 34 − 两 header 30），仓库栏高度变了要同步那个 `fixed` 常量；`git_watch` 是全局滚动窗口——不同 pane 输出共用一个 8KiB 窗口，理论上能拼出一次误命中（后果只是多刷一次）；`REPO_CACHE` 进程级 thread_local 不清理（与原版 Map 同形态）；worktree 弹窗 `create_error` 字段实际恒 None（Rust 无外层 catch，失败全进 `create_results`，字段留着对齐原版结构）；FileTree「查看变更」与项目列表「Worktrees」两入口转 Y 批（`open_file_diff` / `git_worktree::open(discover_repos=true)` 已就绪，只差菜单项+两条菜单序断言同步）。
- **R 批（cb3282d 后）记档**：UI 间距不随 uiFontSize 缩放（原版 Tailwind 的 rem 连内边距一起缩，GPUI 侧间距是像素字面量，10px/20px 极端档观感有差）；uiFontFamily 只取首个族名（gpui `font_family` 单值，整串仍原样落盘）；提示音自定义仅认 .wav——选择时非 wav 出警告条，但**已存的旧值不再提示**；skin（blueprint/fluent2）与终端连字 UI 置灰待底层能力；⚠️ USED_KEYS 大半 key 是动态传进 `t()` 的（section()/toggle_row()/MENU_GROUPS/hotkeys 表），文档注释那条 grep 抓不到，取全表必须连 settings.rs/hotkeys.rs 的 key 字面量一起扫（i18n.rs 表头已加警告）；`AppStore::background_art()` 的 dead_code 标注属误标（main.rs 实际在用）；深链 initial_page 已打通但两处入口都传 None（与原版一致）。
- **N 批（2bb0205 后）记档**：mt-project 无 reveal 语义（mt-app 自落 explorer `/select,` 走 raw_arg 防空格路径二次转义，建议上收 mt_project::editor）；`fs::delete_entry` 是硬删非回收站（文案「无法撤销」相符，后续可接 trash crate）；gpui-component `InputState::select_all` 是 pub(super)，prompt 默认值全选做不到；菜单键盘方向键导航/进场动画未做。
- **弹框宽高对齐批（2026-08-20）记档**：逐一对照原版核过全部弹框宽高并改齐 7 处——confirm/alert 380→360、重命名终端 380→360（原版都走 `.prompt-dialog` 统一 360px）、移除项目确认 420→320（`ProjectList.tsx` 是 w-[320px]）、设置面板正文去掉 640px 定值上限改纯 80vh、SSH 连接/关联 SSH/添加远程项目三面板总高由「头 + 定值 520/380」改按原版口径现算 `min(70vh, 680)`（`ssh_panel::panel_total_h` 三处共用，正文 flex-1，`BODY_H`/`LIST_H` 常量删除）、用量统计浮层边距 24/60 定值改 10vw + 10vh/5vh（= 原版 w-[80vw]+pt-[10vh]+max-h-[85vh] 撑满时的几何）。**有意保留的差异**：移动端面板 540px 与 env_vars 表格区 380px 两处定值——原版是 max-h 内容自适应，gpui Dialog 无自适应语义，定值是内容自然高的近似，改成恒 vh 定高反而在内容少时留大空白；会话查看是面板内预览非独立弹窗（形态差异，session_panel 模块注释已记）；marker 浮层固定 300 vs 原版 min-w 280 自适应。
- **pane 拖拽批（2026-08-20，追平原版 v0.14.0 / PR #49）记档**：gpui 分支基线是 main 的 v0.13.1，缺 PR #49 那批（pane 拖拽移动/合并/重排 + 双击最大化），本批在 GPUI 侧**重新实现**（两边技术栈不同，无法 cherry-pick）。落点：`tree.rs`（`DropZone` + `insert_split_at` + `move_pane_in_layout` + `move_pane_to_tab_index`）、`dnd.rs`（`DragPane` 载荷 + `pane_drop_zone`/`tab_insert_index` 两个几何判档 + `PreviewIcon::Terminal` 拖影图标）、`store.rs`（`move_pane`/`move_pane_to_tab`/`maximized_pane_id`/`toggle_maximized_leaf`，`ProjectState` 加**不落盘**的 `maximized_pane_id`）、`terminal_area.rs`（tab 拖起、tab 栏落点层、终端区四边/中央落点、最大化钮与双击、最大化渲染分支）。逐条记档：
  - **树变换取 `&self` 返新树而不是按值消费**：两个 move 入口都有「落回原位 = 返 `None`」的语义，按值消费会让一次 no-op 把调用方手上那棵树吃掉（store 里 layout 是 `Option`，take 出来拿不回去）。多一次整树 clone 只发生在**用户松手那一下**，可忽略。
  - **`on_drop` 不带位置这条硬约束贯穿全批**：终端区档位与 tab 插入位都由 `on_drag_move` 提前算好存进 `TerminalArea` 的 `pane_drop`/`tab_drop`，drop 时读档；三份拖拽视图态统一与 `cx.has_active_drag()` 与门 + 在 `render` 开头对账清理（与既有 `file_drop_pane` 同一套）。
  - **Esc 取消已补齐，X 批那条记档结清**：`App::stop_active_drag` 是公开 API，配 `capture_key_down` 挂在终端区根容器上即可——按键捕获相沿「根 → 焦点节点」下行，先于 `TerminalView` 把 Esc 翻成 `\x1b` 那一步（与原版 `paneDragState.ts` 那句 capture 监听同一个道理；gpui `dispatch_key_down_up_event` 的捕获相尊重 `stop_propagation`，且 `push_node` 对每个元素都建 dispatch 节点，id-less 的 div 照样在派发路径上）。只在真有 pane 拖拽在飞时吞掉，终端里按 Esc 的行为一字节不变；核对过 gpui-component 的 escape 绑定全是 context 限定（Dialog/Input/PopupMenu…），终端聚焦时无一命中。**2026-09-02 补记**：这一挂法只对 pane 拖拽有效——从文件树起拖时焦点在文件树行上（按下即聚焦），终端区根容器不在派发路径上，Esc 收不到。已把拦截**上移到 Workspace 根**（`main.rs`），判据改为 `cx.has_active_drag()`，四条内部拖拽链路（pane / 文件树→终端 / 文件树内移动 / 项目列表排序）一视同仁；各视图的落点残留仍靠自己 render 里与 `has_active_drag` 的对账清，`stop_active_drag` 自带的 `window.refresh()` 就够。资源管理器拖入的 `ExternalPaths` 不经这里：OLE 拖拽期间 Esc 由拖源处理，gpui 收到 `FileDropEvent::Exited` 自己清 active_drag。同批新增文件树**移动**（拖拽落点 + 右键「移动到 ▸」懒加载多级面板，见 `file_tree/move_to.rs` 模块注释：菜单层本身是 deferred 绘制、gpui 禁止嵌套 defer，子面板只能挂面板根上按上一帧行矩形定位）。
  - **grabbing 光标 Windows 上拿不到（记档不修）**：`CursorStyle::ClosedHand` 在 gpui 0.2.2 的 `platform/windows/util.rs::load_cursor` 落进 `_ => IDC_ARROW`，强设过去反而从手形退化成箭头；而 gpui 拖拽期间本来就把**拖源元素**的 `mouse_cursor` 提升成全窗口光标（`elements/div.rs:1834`），tab 是 `cursor_pointer` → 全程手形，已是 Windows 上最接近 grabbing 的一档。
  - **「落下无动作就不给任何提示」是原版三轮评审的定稿口径，一步到位**：独占一组的 pane 拖自己身上（四边/中央全 no-op）、拖到自己所在组的中央、单 tab 组拖回本组 tab 栏——三种都不画预览/指示线。`zone_has_effect` 的判据与 `move_pane_in_layout` 返 `None` 的条件**严格同集**，不会出现「指示了却静默无动作」。
  - **插入指示线直接抄加强版**：原版 2px 细线经评审实测「肉眼难辨」才改 3px 圆头 + accent 双层光晕（`0 0 6px` + `0 0 2px`），中间那一版不复刻。指示线画在 tab 栏**外层非滚动包装**里——tab 栏是 `overflow_x_scroll`，绝对定位子元素会跟内容偏移，而 x 又由屏幕坐标现算（已含滚动量），放里面就双算了。⚠️ 那层包装必须是**纵向** flex：`bar` 自带 `flex_none`，放进默认横向 flex 会变成「宽度按内容撑」，右侧控件簇的 `ml_auto` 跟着缩到 tab 后面去。
  - **tab 矩形改为每个 tab 常挂量尺 canvas**（原先只挂悬停中的那一个）：插入位要算「指针在哪个 tab 中线哪一侧」，需要**全部** tab 的横向区间。顺带删掉 `tab_hover_rect` 字段，缩略图锚点改读同一张 `tab_rects` 表。`tab_slots` 里有一个 tab 没量到就整份作废——区间与 tab 顺序必须一一对应，缺一个会让后面所有插入位错一格。
  - **原版 `getNodeKey` 稳定性修复的等价坑在 GPUI 侧结构性不存在**：终端实体 `TerminalPane` 按 `pty_id` 挂在 `AppStore::terminals` 表里（旧版 `terminalCache` 的等价物），布局树只存 pane id——移动/重排/最大化都只动树的形状，实体从不被销毁重建，PTY 不断、回滚缓冲不丢。同理原版那段 `suppress-pane-enter`（最大化/还原重挂 PaneGroup 会重播整树淡入）也不需要：进场进度表按 `项目\u{1}叶子` 索引且不按帧回收，同一叶子换个容器渲染拿到的还是早就跑完的那条进度。已在 `tree.rs`/`store.rs`/`terminal_area.rs` 三处就地写清，并有单测「移动保留 pty_id」钉住。
  - **最大化是运行时状态**：`persist.rs` 一个字没动；判据落在**叶子**上而非 pane 上（组内切过 tab 之后再双击仍应还原，拿 pane id 直接比会变成「换成另一个 pane」）；`maximized_pane_id()` 自带「布局是 split」这道闸，`after_layout_change` 顺手清陈旧 id（pane id 进程内单调递增，无复活路径）。`split_pane` 与 `move_pane` 落地前无条件 `clear_maximized`——新格子分进隐藏的整树看不见；`move_pane_to_tab` **不清**（最大化时 tab 栏只能同组重排，结果就在眼前）。
  - **最大化时 `pane_rects` 只留被铺满那一组**：方向导航挑的是「屏幕上相邻的格子」，原版 `findAdjacentPtyId` 查 DOM、卸载掉的 PaneGroup 天然查不到，这里显式把那条性质补回来（`tab_focus`/`tab_rects` 仍按整棵树保留——句柄要跨帧稳定，还原后立刻还要用）。
  - **控件簇由四钮变五钮**：`MARKER_ANCHOR_INSET` 拆出 `marker_anchor_inset(has_maximize)` 分档（最大化钮是**条件出现**的，原版量 DOM 天然不会错，这边靠常量算就必须显式分档），单测同步钉 106 / 130 两档。
  - **双击落点靠子元素自己 `stop_propagation`**：原版是 `e.target.closest('[data-pane-tab],button')` 排除名单，GPUI 侧改由 tab /「+」/ 分屏两钮 / 关整组 / 最大化钮各自截断——效果相同且不必维护「哪些子元素算控件」的名单。
  - **明确不迁移**（原版 React/WebView 特有）：`paneDragState.ts` 整套鼠标自绘链路（根因是 Tauri dragDropEnabled 吃 HTML5 DnD，gpui 无此约束）、stopPropagation/mouseup 传播修复、React key 稳定性修复、`suppress-pane-enter`。
  - **遗留（照 X 批既有决议不修）**：从 tab 上的关闭 `×` 起拖也会拖起整个 tab（原版只豁免 button；X 批对项目行的 `×` 已同款记档「无害」）；起拖阈值 2px（gpui 内建）vs 原版 5px 曼哈顿，手感更灵敏。
  - **i18n**：`paneGroup.{maximizePane,restorePane}` 两条 tooltip 走 TS 源头补 + 重跑 `gen_from_ts.mjs`（741 → 743），`i18n.rs::USED_KEYS` 与 `mt-i18n/tests/consistency.rs` 的对账常量同步。
  - **测试**：`tree.rs` 16 例逐条照抄原版 `tests/paneLayoutOps.test.cjs` 的 15 例（另加「移动保留 pty_id」一条），`dnd.rs` 6 例打两个几何判档（含并列取先出现档、越界钳末尾、零尺寸矩形不判档），`store.rs` 1 例钉最大化三态；mt-app 614 单测 + 4 集成全绿（+23），mt-i18n 12+3 全绿，`cargo build --workspace` 通过。⚠️ 复跑时若 GPUI dev 实例正在运行，`cargo test -p mt-app` 会卡在「无法替换 `target/debug/mini-term.exe`」——先关实例，或用 `cargo test --no-run --message-format=json` 取出 deps 下的测试二进制直接执行。
  - **真机验收点（本批未跑 GUI，E2E 政策暂停中）**：① 拖 tab 到另一组四边看半屏预览与落地方向；② 拖到中央看并入末尾并激活；③ 同组 tab 栏拖动看 3px 光晕指示线位置与落子结果；④ 双击 tab 栏空白与点最大化钮的 toggle；⑤ 最大化下点分屏钮应先自动还原；⑥ 拖拽中按 Esc 应撤销且**不**往终端写 `\x1b`。

- **收尾批：删旧代码 + 发版切换（2026-08-20，用户拍板「停发 Tauri」）记档**：`src/` + `src-tauri/` + 全部 Node 前端基建（vite/tsconfig/package-lock/public/index.html/`.tmp-tests`/旧 Node 测试）物理删除，GPUI 是唯一形态；找旧实现看 git 历史（并入点 `236d5c1`）。迁移与联动：
  - **`src-tauri/mt-sidecars` → 根 `sidecars/`**（git mv 保历史）：仍是独立工作区（版本自成语义、不并入根 workspace——并入会触发依赖统一重排根 Cargo.lock），根 Cargo.toml exclude 换血为 `["sidecars", "relay-server", "mobile"]`；path 依赖改 `../crates/*`；自带 `.gitignore` 忽略 `/target`（旧位置靠 src-tauri 侧忽略链，搬家后会裸奔）。
  - **就位模型简化**：`stage-sidecars.mjs` 不再产 tauri externalBin 三重后缀副本，dev/release 一律裸名就位到主程序所在 `target/<profile>/`（与运行时 `current_exe().parent()` 解析对齐，「目录布局即发布包布局」）；顺带结清旧记档「GPUI dev 下 sidecar 被放进 src-tauri/target/debug 拿不到」的缺口。`stage-conpty.mjs` 缓存迁根 `.conpty-cache/`、默认就位 `target/debug/portable-conpty`。`release-build.mjs`（收 Tauri bundle）删除。
  - **i18n 源头迁入**：`src/i18n/locales/*.ts`（32 文件 743 条）git mv 到 `crates/mt-i18n/locales/`，`gen_from_ts.mjs` 改读 `../locales`；再生成 diff 仅头注释一行，无损。**dict.rs 仍是生成物禁手改**，文案源头从此在 mt-i18n 自己家。
  - **release.yml 重写为三平台 GPUI 矩阵**（用户指示参考旧版三平台）：Windows x64 便携 zip 照旧；macOS arm64 自组 `.app`（Info.plist 手写，bundle id 沿用 `com.mini-term.app`，icns 由 `docs/icon.png` sips+iconutil 现做）打 dmg；Linux x64 出 deb（dpkg-deb 手打，版本 `-`→`~` 保预发布序，二进制落 `/usr/lib/mini-term` + `/usr/bin` 符号链接——`/proc/self/exe` 解链接后 sidecar 同目录解析仍成立）+ tar.gz。`fail-fast: false`：mac/Linux 构建线**未经真机验证**，失败不拖累 Windows。mt-app 补 `[target.'cfg(target_os="linux")']` 段开 gpui `x11`/`wayland` feature（不开则 Linux 构建无 windowing 后端）。
  - **保留改造的两个 Node 测试**：`tests/conptyBundle.test.cjs`（删 tauri.windows.conf.json 映射断言，staging 模块 fixture 测试保留）、`tests/rustCryptoFeatures.test.cjs`（manifest 重指根 workspace——mt-project 的 git2 vendored-openssl 在 macOS ARM 图上已验证成立）。package.json 瘦身为纯脚本入口（零 npm 依赖，`node_modules` 不再存在）。
  - **文档**：CLAUDE.md 全文重写为 GPUI 架构（crate 职责表 + 进程内 PTY 数据流 + AI 判定铁律/Grok 差异等深坑照搬更新引用）；双语 README 改单形态（logo 从 git 历史恢复到 `docs/icon.png`，摘掉连体字与蓝图皮肤两条 GPUI 置灰功能）；features 双语清单同步改写。
  - **Windows 产物改 NSIS 安装包（同日晚，用户指示「改成 nsis」）**：便携 zip 退场，`scripts/windows-installer.nsi` + runner 自带 makensis 出 `*-windows-x64-setup.exe`。安装身份对齐旧 Tauri NSIS——卸载键沿用 HKCU `Uninstall\Mini-Term`、`InstallDirRegKey` 读旧 InstallLocation（键名与值形态取自装机注册表实测：旧版装在自选目录 `E:\Program Files\Mini-Term`），老用户原地升级不留双条目；用户级安装免 UAC，升级/卸载前 taskkill 主程序+三 sidecar（旧版主程序 `Mini-Term.exe` 与新版 `mini-term.exe` 大小写不敏感同名，一并管住），卸载只删自装文件与空目录、AppData 用户数据不碰。配套：mt-app 新增 `build.rs` + winresource（挂 `cfg(windows)` 的 build-dependencies，判宿主，mac/Linux 构建线零影响）把 `resources/icon.ico`（自 git 历史 236d5c1 恢复）与版本信息嵌进 exe——GPUI 不管 exe 资源，不嵌的话快捷方式/资源管理器是白板图标；FILEVERSION 资源只收纯数字四段，1.0.0-beta 拆数字段喂、字符串档保留完整语义版本（installer 侧 VIProductVersion 同理，workflow 里 `-`/`+` 前缀截断补 `.0`）。
  - **测试与残留清查（提交前主会话复核）**：`cargo test --workspace` 26 目标 1369 过 0 败；sidecars 工作区 `cargo build` 通过；仅存两个 Node 测试 4/4 过；`gen_from_ts.mjs` 幂等（再跑 dict.rs 哈希不变）。全仓 tauri/webview 清查：Cargo.lock 三工作区零 tauri/wry/webview 依赖，`.github/workflows/` 只剩 release.yml，代码内命中全为「原 Tauri 版如何」历史对照注释（保留）；修掉两处失实残留——settings 连体字描述里 Tauri 时代的「Windows 完整支持 / macOS·Linux 受 webview API 限制」平台差异尾巴（GPUI 版整行置灰、hint 已另行说明按列摆放暂不支持连字），及 relay-server/protocol 头注释的 `src-tauri` 指向（改指 crates/mt-relay）。
  - **发版结果（同日晚，v1.0.0-beta 三平台全绿）**：tag 共重打三轮才齐——①首轮 Linux 死在 dpkg-deb：bash `${VERSION//-/~}` 的替换串被 tilde 展开成 `$HOME`（转义 `\~` 修）；②次轮 Windows 死在 makensis：windows-latest 镜像**装了 NSIS 但不在 PATH**（按 `C:\Program Files (x86)\NSIS\makensis.exe` 固定位调用 + choco 兜底）；③第三轮四产物齐活（setup.exe 17MB / dmg 30MB / deb 24MB / tar.gz 34MB）。macOS 自组 .app+dmg 线与 Linux apt 依赖清单 + gpui x11/wayland 构建线均一次通过，「首个 tag 要迭代」的预期只应验在打包脚本层。迭代方式实证：workflow 只有 tag 触发且 rerun 用旧提交，只能「改→commit→`gh release delete --cleanup-tag`→重打 tag」整轮重来；中途重打前先 cancel 在跑的旧 run，防其 upload 步骤把旧 commit 产物塞进新壳。
  - **遗留**：真机验收仍未做（E2E 政策暂停中；Windows 侧装一次 setup.exe 即是最直接的真机验收）；mac/Linux 产物未经真机运行验证；`docs/` 下 plans/specs/superpowers/adr 历史文档按惯例不回改。
- **统计面板三处 UI 修复（2026-08-20，用户报）**——三条各自的上游事实值得记档：
  - ~~右键菜单没有高度上限~~ **已修**：`anchored` 的贴边只**平移**不缩放，条目比视口还高时它把顶边钉在 margin 上、下半截溢出窗口外（那些条目点不到）。修法是条目列表另包一层 `max_h + overflow_y_scroll`，**但只给叶子层** —— gpui 的 `Style::overflow_mask` 对**两轴一起**出 ContentMask（`overflow_y_scroll` 也会裁 x），而子菜单是 `absolute left:100%` 挂在父项里的，给带子菜单的层开滚动会把子菜单整块裁没。已知缺口：滚动位置不进视图状态，↑↓ 选到视口外不会自动滚过去。
  - ~~自定义日期框宽度不够~~ **已修**：`gpui_component::Input` 从给定宽度里逐层扣左右各 12 的 padding + 1px 边框 + `RIGHT_MARGIN`(10，给光标留的)，原来的 112 只剩 76px 可视；而它的文字是 **rem 定死的 14px**（`input_text_size` → `text_sm`），**不跟 `ui::font_px` 的 UI 字号缩放走**，等宽字族下 `2026-08-20` 要 84px。改 150 并补 `flex_none`（原来那两层可收缩，挤的时候还会更窄），另配自绘日历浮层 `date_picker.rs`——**不用 `gpui_component::time::DatePicker`**：它的日历图标与翻月箭头全走 `AssetSource`，0.5.1 的 crate 包里一个 svg 都没有（上游把 lucide 放在示例程序的资产目录），渲染出来是三块空白且编译期无感。顺带：`DeferredDraw` **不保存 `content_mask_stack`**，所以 `deferred` 浮层挂在抽屉式面板里也不会被抽屉裁掉，不必像 `menu.rs` 那样做成全局层。
  - ~~趋势图面积有锯齿~~ **已修（真几何 bug，不是抗锯齿）**：`chart.rs::band_top_edge` 把曲线采样点**逐顶点钳位**进渐变横带却不在穿带处插交点 —— 折线先钳顶点再连线 ≠ 折线被钳出来的形状。陡坡上两个相邻采样点跨好几条带时，每条带都把「本该只占一小截 x」的斜边摊到整段 x 上：同一列各带只填自己下半截，拼出来是十几条横纹（总面积仍是对的，所以肉眼就是「曲线下方一排锯齿」且色块溢到曲线上方）。修法是逐段求与 `lo`/`hi` 的交点、`t ∈ (0,1)` 才插。参考：Windows 侧 path 走 4×MSAA 渲进中间纹理再 resolve（`directx_renderer.rs`），缓坡上的 4 级覆盖度是 1px 级毛边，量级比这个小一个数量级。同批把 `samples_per_segment` 上限 8 → 24（7 天视图一格 70px，8 段折线拟合三次曲线在弯处看得出是折的；总点数照旧由 `MAX_CURVE_POINTS=600` 兜住）。
  - **遗留**：三条都只过了单测与编译，**未做真机观感验收**（E2E 政策仍暂停）。
- **Wave 1 D 批的漏搬追记（2026-09-02，合 PR #73 顺带发现）**：D 批验收记的是「hook 体系逐字搬运 ✅」，但核的只是**模块内容**、没核**装配点**——`src-tauri/src/lib.rs::setup` 里那句 `std::thread::spawn(|| { sync_claude_hooks_if_registered(); sync_grok_hooks_if_registered(); })` 没跟着搬进 `mt-app::main`，两条「已注册用户的启动期自愈」自收尾批（`b52a654` 物理删 src-tauri）起就成了只有定义、没有调用的死代码，13 天无人发现。
  - **编译期零信号**：两者都是 `pub fn`，跨 crate 的未调用公开项不触发 `dead_code`；测试也测不出来——被测的是函数本身，不是「有没有人调它」。
  - **实际后果**：Grok 用户 `{grok_home}/hooks/` 里的 hook 二进制副本从此再没刷新过（mini-term 升级后一直留着旧副本，而那正是这条自愈唯一的存在理由）；Claude 那条恰好一路空转（`CLAUDE_HOOK_EVENTS` 期间没长过，`missing` 恒空直接 return），属侥幸未暴露。
  - **修法**：`71eee1e` 把调用点补回 `main()` 启动期，与同期新增的 codex feature 键迁移（`codex_hooks` → `hooks`，issue #72）合成同一个后台任务按序跑。三条的第一道闸都是「配置里有没有我们的条目」，没注册过的用户一律不碰。
  - **教训（下次逐字搬运批次照做）**：`pub fn` 的跨 crate 搬运，编译器不会为漏搬的调用点报任何错，验收清单里必须显式带上「原调用点搬到哪儿了」这一条——只对账「模块内容一字不差」是不够的。

## Wave 5 批次排程（2026-08-19 主会话规划；当前为用户指示的暂停点，下午继续）

编排规矩（用户指令，已入长期记忆）：开发一律 Opus subagent；同时运行 subagent **≤3**；后台静默等通知、不主动读运行中 agent 输出；禁止 agent 套娃派子 agent；不做 E2E。节奏：交付 → 主会话独立复跑测试验收 → 提交 → 补位派下一批。跑 dev 实例给用户看效果用 `MT_APP_DATA_DIR=%LOCALAPPDATA%\mini-term-gpui-dev`。

| 批 | 内容（审计条目） | 任务书（docs/batch-specs/） | 派发前决策 / 前置 |
|---|---|---|---|
| R ✅（`cb3282d` 2026-08-19） | 设置面板 9 分页 + skin 色表（#19 + #5 剩余） | settings-pages.md | 已按决策落地：连字/皮肤渲染但置灰+说明词条（**皮肤那一栏已于 2026-08-25 整段移除**，见上方 J 批记档）；UI 字号字族 thread_local 快照真接上（84 处 text_size 换 ui::font_px）；about 页复用 zed-reqwest；原语全自绘；另收编键位表 hotkeys.rs 为唯一事实来源 |
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

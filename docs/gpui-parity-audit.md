# GPUI 迁移 UI/UX 全量对照审计（缺口任务书）

> 2026-08-18 由审计 agent 逐文件对照产出，主会话整理。基线：Tauri 前端 `src/`（18171 行 TS/TSX，44 组件）
> 对 GPUI `crates/mt-app/src/`（16 模块，含 G 批六个新模块，约 6400 行）。
> 本文档是后续开发批次的**任务书索引**：每派一批，从「缺口汇总」挑条目展开；做完把状态改 ✅ 并注提交号。
> 状态图例：✅ 已对齐 · 🟡 部分 · ❌ 缺失

## 结构性事实（grep 证实）

- `crates/mt-app/Cargo.toml` 声明了 `mt-relay.workspace = true`，但 `crates/mt-app/src/` 里 `mt_relay` 零引用——依赖挂着、代码没接
- mt-app 没有 `mt-i18n` 依赖，全部文案硬编码中文字面量（约 80 处）
- `theme_bridge` / `switch_to_theme_pack` 在 mt-app 零引用；`main.rs` 钉死 `ThemeMode::Dark`，`ui.rs` 逐值硬编码暗色
- `crates/mt-ui` 的 `TerminalView`（IME 全套）已完整实现且注释含四步接线法，但 `pane.rs` 仍直接用 `TerminalElement` → 中文输入法不可用

## 缺口汇总（按层次与依赖排序）

### 第 0 层：接线型——crate 已就绪，只差调用（性价比最高）

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 1 | **IME 接线**：`pane.rs` 换用 `mt_ui::TerminalView`；`clear_preedit()` 两调用点（activate_pane/dispose_terminal）；OSC 应答改 `terminal_color_rgb` | 小 | mt-app | ✅ J 批。gpui 派发顺序（action 先于 key 监听）已实证等价原版 capture-consume，机制写进 pane.rs 注释 |
| 2 | **快捷键对齐**：ctrl-shift-b 修正 / ClosePane→close_leaf（关整组）/ 补 Ctrl+Tab、Ctrl+Shift+Tab、Ctrl+1..9 | 小 | mt-app | 🟡 J 批完成本体；P 批补齐 terminalSearch=Ctrl+F / globalSearch=Ctrl+Shift+F / switchProject=Ctrl+Shift+P（键位逐条对 `hotkeys.ts`）；剩 markerPrev/Next 随 marker 批（#25）；Ctrl+Shift+A/U/J 三条原版没有、保留并已注明 |
| 3 | ~~窗口聚焦即清未读~~ | 小 | mt-app | ✅ G 收尾已补 |
| 4 | **i18n 接线** | 中 | mt-app + mt-config | ✅ L 批。90 调用点 84 key 覆盖 8 文件，key 逐条照 TSX 抄并有「84 key×双语全在」测试防字典重生成漂移；AppConfig.locale 存 String 防手改坏值拖垮整份 config；首启 Win32 探测不落盘（对齐 detectInitialLang 跟随系统）；gpui-component 桥必须传 bcp47 而非 code（其 ui.yml 键是 zh-CN，传 zh 会静默回英文）；语言切换入口在设置对话框；ElementId 改用 key() 防随语言变。**留 7 条缺 key 文案（TODO(i18n) 注释在位）+1 条 pricingLocalHint → 转 M 批走 TS 源头补+重生成** |
| 5 | **主题桥接线** | 中 | mt-app + mt-ui | 🟡 J 批：theme.rs 唯一装配入口（light/dark/auto + customThemeId 失败回落只清内存不落盘）、ui.rs 改 Palette（dark/light 逐值抄 styles.css、from_pack 对齐 buildTokenMap，函数签名零改动走 thread_local）、terminalFollowTheme 含存量终端热更、切换 pub 入口全备且起 PTY 前装配。剩：`config.skin` 皮肤色表（blueprint/fluent2）、appearance 设置页 UI（→#19）、背景图渲染（→K 批 mt-ui） |
| 6 | **AI 自动 resume** | 中 | mt-app | ✅ J 批。resume_pending 置位只看 ai_session（遵原版：开关关着也保留标记）；磁盘格式零改动有测试钉住；pane 自带 cwd 优先防 worktree 带偏；lookup_ai_session_cwd 为同步调用（仅存量无 cwd 记录触发，理论卡顿已记档） |

### 第 1 层：基建型——挡住一大片功能

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 7 | **上下文菜单基建** | 中 | mt-app | ✅ N 批自建 menu.rs（gpui-component 菜单四条硬伤记档：IconName 无资产/无 danger 态与快捷键标签/配色走它自己的 theme token/ContextMenu 包装器过不了 prefers_local_handling 闸门）；样式逐条对照 styles.css，勾选「✓ 」文本方案与原版一字不差；焦点开时收走关时先还再跑动作。遗留清零（polish 批）：键盘方向键导航 ✅、menuPopIn 进场动画 ✅；「遮罩吃掉关闭那一下点击」维持记档不修（与原版差一次点击，自恢复） |
| 8 | **拖放基建**——项目拖拽排序、拖文件夹加项目、拖文件进终端（外部资源管理器 + 内部 FileTree 两条链路 + 虚线高亮框） | 中 | mt-ui + mt-app | ✅ X 批（合并 `266696e`）。内外拖统一 gpui on_drop（dnd.rs）；⚠️ on_drag_move 无 hitbox 判定、命中闸必须自做（hit_ratio），已进注释；Esc 取消内部拖拽未做（记档） |
| 9 | **图标体系** | 中 | mt-ui | ✅ K 批组件（BrandIcon 11 家/TechIcon 12 种/FileIcon 53 类/StatusDot 四态勾叉+900ms 旋转；全自绘矢量，判据在 vector.rs 头注释）+ M 批消费（状态灯/tab 与会话面板品牌图标含 or_else(infer) 补 opencode/pi、项目列表 TechIcon、文件树 FileIcon、边条 44px 图标化逐点照抄 ActivityBar SVG + 全局 AI 徽标）。留档清零：dirKinds 探测 ✅ tail-lists 批（探测缓存+三消费点）、文件树 git 着色 ✅ Y 批、项目行状态灯位置 ✅ Y 批 |
| 10 | **通用命令式弹窗 + 关闭确认** | 小 | mt-app | ✅ N 批。prompt/confirm/alert + open_guarded 按种类防叠开（不同种类可叠，摘表挂 on_close 五条关闭路全覆盖）；关闭确认盘点口径逐字照原版（status∈{ai-working,ai-idle} 不看 ai_session 身份），四条关闭路径统一走 pane_actions，确认后按 id 从最新布局重取；缺 2 词条 fileTree.dialog.createFailed*（原版该处也是静默失败，非回退） |
| 11 | **终端滚动条** | 中 | mt-ui | ✅ K 批。6px 样式照抄 styles.css；alt screen 不画；不打穿 damage 缓存（只在 paint 发 quad）；滑块分母 total-screen；闲置按时间算 alpha 淡出（不用 with_animation 防持续请求帧）、回看中保留 50% 残留；宿主零改动默认生效 |

### 第 2 层：面板补全

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 12 | 项目列表补全 | 中 | mt-app | ✅ Y 批（`f609bfd`）落地：行内重命名（项目+分组）、AI 厂商图标堆叠、worktree ⎇ 徽章与失效子项目清理、DoneTag、accent 竖条恒占位、底部按钮条（SSH 钮随 #28）、菜单补 Worktrees。键盘导航 ✅ tail-lists 批（F2 走 main.rs 唯一处理器按持焦分流）、hover 250ms 缩略图 ✅ tail-lists 批（MiniTerminalElement）。剩：菜单 SSH/环境变量项（随 #28/#30） |
| 13 | **项目分组**：分组行渲染 + 折叠 + createGroup/removeGroup/renameGroup/toggleGroupCollapse/moveItem 五个 action + 拖拽 + 分组右键菜单（`ProjectTreeItem::Group` 已有数据从不渲染） | 大 | mt-app | ✅ X 批（合并 `266696e`）。project_tree.rs 纯函数 41 测；磁盘格式一字不改与 Tauri 互读；分组重命名暂走弹窗（行内编辑归 Y 批）；底部三按钮归 Y 批 §C.3；原版 moveItem 丢子树 bug 已兜底 |
| 14 | 文件树补全 | 中 | mt-app | ✅ Y 批（`f609bfd`）落地：git 状态着色（含目录汇总，git_watch 多订阅者触发+isAiPty 跳过）、头部三按钮、三态、单链目录压缩（链深 8 封顶）、「查看变更」菜单项；键盘导航 ←→ ✅ tail-lists 批 |
| 15 | tab 栏补全 | 中 | mt-app | ✅ Y 批（`f609bfd`）落地：新建按钮 shell 选择菜单（<=1 不弹照抄）+ 横向滚动；hover 缩略图 ✅ tail-lists 批 + 分支会话两项 ✅ fork 批 |
| 16 | 终端右键菜单 | 中 | mt-app + mt-ui | ✅ fork 批（合并 `c337716`）补齐分支段：「分支到新分屏/查看会话分支/未获身份置灰提示」tab 与终端右键同源判据同份实现（branch_entries_for_pty）。唯一遗留：SSH 子菜单段（等 mt-ssh 进 crates/，#28） |
| 17 | 用量面板：custom 自选起止日期 + 自动刷新档位（0/5/10/30/60s 默认 5s）+ 单项目下拉（现仅当前项目开关）+ 排行条点击切 scope + Top 会话点开查看 + 骨架屏/数字滚动 + 偏好持久化 + **价格表拉取**（原版 fetch models.dev，Rust 侧需 HTTP 依赖，现读本地 model-pricing.json 缺省按 $0） | 中 | mt-app | ✅ Q 批。pricing.rs 走「手工裸表→新鲜缓存→拉网(zed-reqwest blocking，净新增 crate=0)→过期缓存→报错」，canonical 建键+全序比较器+0/0 占位丢弃；定时器挂 set_visible 闸；六相位纯函数分派、价格未就绪不渲染 KPI；custom 日期用 Input+纯函数闸门（无日历弹层，原版是浏览器原生 date input）。留项清零（tail-anim 批）：趋势图面积曲线/网格/双轴刻度 ✅（mt_ui::chart 自绘）、.usage-fade-in 与排行条宽度补间 ✅（mt_ui::motion 一次性过渡基件） |
| 18 | 会话面板：平铺⇄树视图切换 + scan_session_lineage 分支连线 + live pane 状态点跳转 + 品牌图标 + spinner；（SSH 远程来源等 mt-ssh 进 crates 后再说） | 中 | mt-app | 🟡 Q 批完成本体（树视图/lineage 连线走 session_branch.rs 纯函数、挂 visible/stale 惰性闸+request_id 守卫、live 三条件 find_live_session_pane、跳转带 claude cwd 反查并回写 ai_session、四项右键菜单收编行内按钮、WSL spinner 自绘）。fork 批（合并 `c337716`）已清：BranchFamilyPanel（menu.rs 自定义元素子菜单能力+单支家族过滤）与 remoteResumeUnsupported 的 alert→toast 回换。唯一遗留：SSH 远程来源（等 mt-ssh，#28） |
| 19 | 设置面板剩余 9 分页：clipboard / appearance / font / ai-notification / ai-hook / system / editor / shortcuts / about（通知三开关现只能手改 config.json）+ 连字/scrollback/UI 字号字族/终端字号热更新（现只作用于新终端） | 大 | mt-app | ❌ |

### 第 3 层：整块新功能

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 20 | **自定义标题栏**：拖拽区 + 最小化/最大化/关闭 + Win11 贴靠（set_max_button_rect）+ 项目切换胶囊 + 全局状态灯（~~窗口标题带版本号~~ ✅ J 批已改 `Mini-Term v{ver}`） | 大 | mt-app | ❌ |
| 21 | **系统托盘**：三色灯 + 右键项目菜单 + 点击定位 + trayStatusEnabled/trayMaxProjects/trayClickFocus（config 字段已在；`unread_done_count()`/`next_attention_target()` 是现成消费口） | 大 | mt-app（Windows API） | ✅ T 批（合并 `8b1cc30`）。独立线程 Win32 托盘+相位机闪烁+签名去重；三开关立即生效；⚠️ Win32 层待真机三查（图标/emoji 菜单/HiDPI），记档见进度看板 |
| 22 | **移动端中转**：mt_relay 实际接线（RelayHost/RelayEvents 实现）+ MobileRelayModal（地址/密钥/状态徽章/配对二维码/重置）+ AiLauncherSection CRUD + 5 条事件落点（status/pairing-code/start-session/rename-pane/会话结构同步）+ store 补 rename_pane_by_id | 大 | mt-app | ✅ U 批（`d3ad441`，合并 `dccbc4f`）。全套落地：RelaySignal 泵/150ms 去抖结构同步/自绘二维码/启动器 CRUD；镜像 opencode/pi 跳过规则经 AiBridge 如实透传不被绕开；记档见进度看板 U 批段 |
| 23 | 终端查找 | 中 | mt-ui + mt-app | ✅ O 批 mt-ui 引擎 + P 批宿主接线：引擎常驻 `TerminalPane`（关键词活过开关）、查找条是终端容器里的 `absolute` 子元素（旧版那条 rAF 定位轮询整个不需要）、Esc/✕ 关闭时 `window.focus(&pane.focus)` 还焦点。⚠️ Ctrl+F **必须绑 action 不能绑 pane 容器的 on_key_down**（TerminalView 认得 Ctrl+F 并 stop_propagation，key 监听是从焦点节点往上冒泡）——search_bar.rs 模块注释已就地更正。偏差两条：逐 pane 一条（原版是 portal 单例）、Ctrl+F 不是 toggle（照原版 `openTerminalSearch` 只开不关） |
| 24 | 全局搜索 UI（SearchModal，后端 mt-project/search.rs 669 行已就绪，Ctrl+Shift+F） | 中 | mt-app | 🟡 P 批。`start_search` 起专用后台线程 + `futures::mpsc` 回主线程（**不占 background_executor**——那是给会 await 的 future 用的）；换搜索直接换掉前台任务，旧结果自然到不了，不必比对 searchId；1000 条封顶、按文件分组保序、`.*` 开关、四态状态条全部照抄且有单测。剩：点结果开 `FileViewerModal`（依赖 #29，现退到原版**双击**那条动作=外部编辑器打开）、分组头 sticky（gpui 无 sticky）、结果列表无虚拟化（原版也没有） |
| 25 | AI 任务 marker 体系（markersByPty store + 按钮 + 浮层 + markerPrev/Next 快捷键） | 中 | mt-app | ✅ W 批（`c15d4e0` 合并 `dfe612d`）。锚点算术推导+饱和整份作废；marker 活得过 TUI excursion（优于原版退 alt 全清）；回滚装满后该 pane 停摆已拍板不做文本重定位补路 |
| 26 | ProjectSwitcher（Ctrl+Shift+P 模糊匹配 + 高亮 + 键盘导航） | 中 | mt-app | ✅ P 批。子序列模糊匹配 + 分组路径兜底匹配 + 命中字符高亮 + ↑↓ 环形导航 + Enter 切项目 + Esc 关闭，逐条照 TSX；分组路径从 `config.project_tree` 现算（项目分组 #13 还没做，但读它不需要）。⚠️ 方向键必须用 `"ProjectSwitcher > Input"` 谓词绑 action —— 与 `Input` 自带的 `up`/`down` **同深度**才压得过它（单行输入框那两个处理器 return 且不 propagate，容器上的 on_key_down 永远收不到），机制有单测钉住 |
| 27 | **Git 全套 UI**：GitChanges/GitHistory/CommitDiffModal/DiffModal/GitWorktreeModal + 右抽屉 sessions⇄git 互斥切换（后端 git.rs 1559 行已就绪）。~~BranchFamilyPanel~~ 系审计误归——它是 AI 会话家族树（scan_session_lineage），已划回 #18 的 fork 批遗留 | 大 | mt-app | ✅ V 批（`a7e9e85`，合并 `9c70bde`）。六组件+拓扑图+pty-output 输出旁路全落地；遗留两入口转 Y 批：FileTree「查看变更」与项目列表「Worktrees」（open_file_diff/git_worktree::open 已就绪，只差菜单项+菜单序断言同步） |
| 28 | **SSH 全套 UI**：SshModal/SshAssocModal/AddRemoteProjectModal/远程项目/断线重连覆盖层/exitedPtyIds 体系（依赖 mt-ssh 进 crates/，属收尾阶段联动件） | 大 | mt-app | ❌ |
| 29 | 文件预览与编辑器（FileViewerModal/CodeEditor） | 大 | mt-app | ✅ AA 批（合并 `a2295a5`）。tree-sitter 多语言高亮（cc 降级 1.2.67 解锁）、CRLF 往返真文件测试钉住、SearchModal/文件树两入口回接。剩记档：Markdown 链接/跳转历史栈整块缺（gpui-component 链接无回调口，待上游）、HTML 只源码态、avif 兜底「默认工具打开」 |
| 30 | 壳层杂项：关窗确认（盘点活 AI 会话列名）+ 版本检查/更新提醒 + FirstRunGuide 完整版（两入口+键位提示）+ 长文本粘贴转文件（4 配置字段已在）+ WSL 启动器重写提示 + 启动埋点 + dirKinds 技术栈探测缓存 + pane 进场动画 | 中 | mt-app | 🟡 Z 批（合并 `2dbc52f`）：关窗确认、自建 toast、双音 WAV、长粘贴转文件（SSH 分支随 #28）、WSL 覆盖提示、smartCopyPaste。tail-shell 批（`5b98de1`）：版本自检+ActivityBar 更新按钮（纠正口径：原版是**有新版本才出现的独立按钮**非设置钮红点，圆点闪烁待 reduce 闸）、FirstRunGuide（🟡 缺 SSH 远程入口随 #28）、启动埋点（本地 stderr 计时非遥测，单进程砍掉 epoch 对齐半场）。dirKinds 探测 ✅ tail-lists 批、pane 进场动画 ✅ tail-anim 批（淡入不缩放，理由见进度看板记档）。非 SSH 部分至此全清，剩 FirstRunGuide SSH 入口与长粘贴 SSH 分支随 #28 |

### 其他细项（散落，随所在批次带走）

- ActivityBar：🟡 M 批已成 44px 图标栏（PANEL/SESSIONS/STATS/SETTINGS 四钮逐点照抄原版 SVG + accent 竖条 + 全局 AI 徽标 + 跳完成钮）；SSH/移动端/Git 入口与更新红点随对应功能批加，徽标闪烁动画未做
- ~~右抽屉应为悬浮层~~ ✅ Q 批：absolute right-0 + occlude + shadow，画在三栏后弹窗前（对应原版 z-45）；自建 6px 左缘拖拽手柄（移动/松手挂根容器，松手才落宽度）；开合不再触发 PTY resize；对照源码确认原版 RightDrawer 不压 overlayStack，GPUI 同步不进 overlay 栈。注：用量面板按源码实况保持居中 Modal（原版 UsageStatsModal 本就不在抽屉里，审计此前括注有误）；抽屉标题栏 chrome（sessions|git 分段+✕）留 V 批
- ~~Toast 五小项~~ ✅ Z 批自建 toast.rs 全覆盖（主会话复核 2026-08-19 夜）：悬停暂停=丢定时器句柄且移开重计满 5s、MAX_VISIBLE=5 超出排队不丢、WslInfo/MobileSession/PasteError 三 kind、点击按 jumps_to_project 跳项目、× 关闭
- ~~Modal 行为：无 overlayStack 快捷键让路；同一 modal 可叠开（缺 isOpen 守卫）~~ ✅ N 批（防叠开）+ P 批（让路）：`overlay.rs` 是唯一的覆盖物栈，弹窗/右键菜单/终端查找条全部登记；全局 action 处理器开头两道闸——① `has_focused_input`（等价原版 `isTypingTarget`，终端不是 Input 所以在终端里敲字不受影响）② 覆盖物压着让路，白名单只有 openSettings / globalSearch。**Esc 只关最上层在 GPUI 里是结构性免费的**（按键沿焦点链派发），原版那套栈顶判定不必复刻
- store 缺失 action：renameProject / setProjectDescription / renamePaneById / exitedPtyIds 系列 / markers 系列 / dirKinds / pendingFork 系列 / collectAiProjects / addProject 的 parentProjectId
- 提示音：Win32 PlaySoundW 只认 .wav（原版支持 mp3/ogg）；无自定义音时回落 MessageBeep 而非原版 880→660Hz 双音
- ~~空态右键弹 shell 菜单~~ ✅ tail-shell 批（`5b98de1`）——审计口径纠正：原版无空态右键处理（main.tsx 全局 preventDefault），实为空态「+ 新建终端」按钮**左键**弹 shell 菜单，已与 tab 栏「+」同源实现；tab 键盘可达（Enter/Space）随 tail-lists 批
- ~~孤儿 PTY 回收~~ **已评估不做**（tail-shell 批）：GPUI 单进程无「页面重载旧 PTY 失引用」链路（PtyManager 注册表结构性缺席+TerminalPane/PtySession 双层 Drop+异常退出由内核收 HPCON/管道句柄、conhost 自退）；脱离控制台的孙进程两侧同款管不到，非迁移缺口
- 链接点击（OSC 8/URL）：**两侧都没有**，非迁移缺口，不追

### 已排除的伪缺口（审计纠错，别再当任务）

- ~~鼠标上报接线~~：已完成（`pane.rs` 的 `.on_input()` 覆盖 down/move/up/wheel，三模式三编码全通）
- ~~tab 拖拽排序~~ / ~~中键关闭 tab~~：原版 Tauri 也没有这两个功能
- ~~分屏比例跨重启回均分~~：G 收尾已修（首帧只量尺、下一帧铺分屏树）

## 性能观察（后续量化）

gpui-component 的 `ResizableState::update_panel_size` 每帧 `cx.notify()`，`ResizablePanel::render` 又 `state.read(cx)`——视图每帧被判失效可能持续重绘。三栏 + 分屏树全用 resizable，需实测 CPU/GPU；改不动上游就考虑自绘分隔条。

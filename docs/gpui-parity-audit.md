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
| 1 | **IME 接线**：`pane.rs` 换用 `mt_ui::TerminalView`（view.rs 注释有四步改法）；切 tab/关 pane 调 `clear_preedit()`；OSC 应答改 `terminal_color_rgb` | 小 | mt-app | ❌ |
| 2 | **快捷键对齐**：`ToggleMiddleColumn` 应为 `ctrl-shift-b`（现错绑 `ctrl-b`）；`ClosePane`(Ctrl+Shift+W) 应调 `close_leaf`（关整组）而非 `close_pane`；缺 nextPane(Ctrl+Tab)/prevPane/selectPaneN(Ctrl+1..9)/switchProject(Ctrl+Shift+P)/globalSearch(Ctrl+Shift+F)/terminalSearch(Ctrl+F)/markerPrev/markerNext 共 8 条；GPUI 多出 3 条原版没有的（Ctrl+Shift+A/U/J，保留） | 小 | mt-app | ❌ |
| 3 | ~~窗口聚焦即清未读~~ | 小 | mt-app | ✅ G 收尾已补 |
| 4 | **i18n 接线**：mt-app 挂 mt-i18n 依赖（根 Cargo.toml 由主会话加行）；启动 `set_locale(cfg.locale)` + `add_locale_observer` 桥接 gpui-component 的 rust-i18n；AppConfig 加 locale 字段；首启检测走 Win32 GetUserDefaultLocaleName；替换约 80 处硬编码文案；语言切换入口 | 中 | mt-app + mt-config | ❌ |
| 5 | **主题桥接线**：`switch_to_theme_pack`/`install_gpui_theme` 调起来；`ui.rs` 常量表改读主题；亮/暗/auto 切换（config.theme）；皮肤 skin；外置主题包 customThemeId + 失败回落；背景图渲染（theme_bridge 已备好 BackgroundArt 数据，mt-ui 渲染未做）；terminalFollowTheme + 全终端配色热更新（现 `store.rs` 固定 `TerminalTheme::default()`） | 中 | mt-app + mt-ui | ❌ |
| 6 | **AI 自动 resume**（aiAutoResume）：`tree.rs::PaneState` 加 `resume_pending`；`persist.rs` 恢复时置位；`hydrate_project` 以会话 cwd 起 PTY 并写 `claude --resume {id}\r`；反查回写 | 中 | mt-app | ❌ |

### 第 1 层：基建型——挡住一大片功能

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 7 | **上下文菜单基建**（gpui-component popup_menu）——挡住项目列表 12 项、文件树 8 项、tab 7 项、终端 5 项四处右键菜单 | 中 | mt-ui/mt-app | ❌ |
| 8 | **拖放基建**——项目拖拽排序、拖文件夹加项目、拖文件进终端（外部资源管理器 + 内部 FileTree 两条链路 + 虚线高亮框） | 中 | mt-ui + mt-app | ❌ |
| 9 | **图标体系**（BrandIcon 厂商 / TechIcon 技术栈 / 文件图标 / SVG 状态灯勾叉字形 + 旋转动画）——现用 "CL/CX/GK" 文本与三形圆点代替 | 中 | mt-ui | ❌ |
| 10 | **通用命令式弹窗**（prompt/confirm/alert）+ 关闭 pane/整组的 AI 感知确认框（现直接关不确认） | 小 | mt-app | ❌ |
| 11 | **终端滚动条**（mt-ui/terminal 现只有滚轮回看，无滚动条） | 中 | mt-ui | ❌ |

### 第 2 层：面板补全

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 12 | 项目列表：右键菜单 12+ 项 + 内联重命名（F2）+ 键盘（Enter/Delete）+ 底部三按钮 + 领位图标 + AI 厂商图标堆叠 + worktree 徽章/子项目缩进 + hover 250ms 缩略图 | 中 | mt-app | ❌ |
| 13 | **项目分组**：分组行渲染 + 折叠 + createGroup/removeGroup/renameGroup/toggleGroupCollapse/moveItem 五个 action + 拖拽 + 分组右键菜单（`ProjectTreeItem::Group` 已有数据从不渲染） | 大 | mt-app | ❌ |
| 14 | 文件树：右键菜单 8 项（后端 fs.rs 已备）+ 头部三按钮（搜索/刷新/外部编辑器选择器）+ loading/错误态 + 键盘导航 + git 状态着色 + 根级单链目录压缩 + 技术栈图标 | 中 | mt-app | ❌ |
| 15 | tab 栏：右键菜单 7 项 + 新建按钮 shell 选择菜单 + 横向滚动 + hover 缩略图 + AI 品牌图标 + 关闭确认 | 中 | mt-app | ❌ |
| 16 | 终端右键菜单（复制/粘贴/fork 会话/分支树/SSH 子菜单）+ 拖选**停留 1s**自动复制（selectionAutoCopySecs 可配，现为松开即复制）+「已复制」气泡 | 中 | mt-app + mt-ui | ❌ |
| 17 | 用量面板：custom 自选起止日期 + 自动刷新档位（0/5/10/30/60s 默认 5s）+ 单项目下拉（现仅当前项目开关）+ 排行条点击切 scope + Top 会话点开查看 + 骨架屏/数字滚动 + 偏好持久化 + **价格表拉取**（原版 fetch models.dev，Rust 侧需 HTTP 依赖，现读本地 model-pricing.json 缺省按 $0） | 中 | mt-app | ❌ |
| 18 | 会话面板：平铺⇄树视图切换 + scan_session_lineage 分支连线 + live pane 状态点跳转 + 品牌图标 + spinner；（SSH 远程来源等 mt-ssh 进 crates 后再说） | 中 | mt-app | ❌ |
| 19 | 设置面板剩余 9 分页：clipboard / appearance / font / ai-notification / ai-hook / system / editor / shortcuts / about（通知三开关现只能手改 config.json）+ 连字/scrollback/UI 字号字族/终端字号热更新（现只作用于新终端） | 大 | mt-app | ❌ |

### 第 3 层：整块新功能

| # | 缺口 | 规模 | 落点 | 状态 |
|---|---|---|---|---|
| 20 | **自定义标题栏**：拖拽区 + 最小化/最大化/关闭 + Win11 贴靠（set_max_button_rect）+ 项目切换胶囊 + 全局状态灯 + 窗口标题带版本号 | 大 | mt-app | ❌ |
| 21 | **系统托盘**：三色灯 + 右键项目菜单 + 点击定位 + trayStatusEnabled/trayMaxProjects/trayClickFocus（config 字段已在；`unread_done_count()`/`next_attention_target()` 是现成消费口） | 大 | mt-app（Windows API） | ❌ |
| 22 | **移动端中转**：mt_relay 实际接线（RelayHost/RelayEvents 实现）+ MobileRelayModal（地址/密钥/状态徽章/配对二维码/重置）+ AiLauncherSection CRUD + 5 条事件落点（status/pairing-code/start-session/rename-pane/会话结构同步）+ store 补 rename_pane_by_id | 大 | mt-app | ❌ |
| 23 | 终端查找（TerminalSearchBar 浮条 + Aa/ab/.* + 搜索引擎） | 中 | mt-ui + mt-app | ❌ |
| 24 | 全局搜索 UI（SearchModal，后端 mt-project/search.rs 669 行已就绪，Ctrl+Shift+F） | 中 | mt-app | ❌ |
| 25 | AI 任务 marker 体系（markersByPty store + 按钮 + 浮层 + markerPrev/Next 快捷键） | 中 | mt-app | ❌ |
| 26 | ProjectSwitcher（Ctrl+Shift+P 模糊匹配 + 高亮 + 键盘导航） | 中 | mt-app | ❌ |
| 27 | **Git 全套 UI**：GitChanges/GitHistory/CommitDiffModal/DiffModal/GitWorktreeModal/BranchFamilyPanel + 右抽屉 sessions⇄git 互斥切换（后端 git.rs 1559 行已就绪） | 大 | mt-app | ❌ |
| 28 | **SSH 全套 UI**：SshModal/SshAssocModal/AddRemoteProjectModal/远程项目/断线重连覆盖层/exitedPtyIds 体系（依赖 mt-ssh 进 crates/，属收尾阶段联动件） | 大 | mt-app | ❌ |
| 29 | 文件预览与编辑器（FileViewerModal/CodeEditor） | 大 | mt-app | ❌ |
| 30 | 壳层杂项：关窗确认（盘点活 AI 会话列名）+ 版本检查/更新提醒 + FirstRunGuide 完整版（两入口+键位提示）+ 长文本粘贴转文件（4 配置字段已在）+ WSL 启动器重写提示 + 启动埋点 + dirKinds 技术栈探测缓存 + pane 进场动画 | 中 | mt-app | ❌ |

### 其他细项（散落，随所在批次带走）

- ActivityBar：现是 14px 中文字「会/量/设」，应为 44px 图标栏 8 按钮 + accent 指示条 + 全局 AI 徽标闪烁；缺 SSH/移动端/Git 入口与更新红点
- 右抽屉应为**悬浮层**（absolute 覆盖在终端上），现做成了第三栏
- Toast：缺悬停暂停 / × 关闭 / 最多 5 条 / wsl-info、mobile-session、paste-error 三种 kind / 点击跳项目细节
- Modal 行为：无 overlayStack 快捷键让路；同一 modal 可叠开（缺 isOpen 守卫）
- store 缺失 action：renameProject / setProjectDescription / renamePaneById / exitedPtyIds 系列 / markers 系列 / dirKinds / pendingFork 系列 / collectAiProjects / addProject 的 parentProjectId
- 提示音：Win32 PlaySoundW 只认 .wav（原版支持 mp3/ogg）；无自定义音时回落 MessageBeep 而非原版 880→660Hz 双音
- 空态右键弹 shell 菜单；tab 键盘可达（Enter/Space）
- 孤儿 PTY 回收（kill_all_ptys 等价物）：GPUI 单进程风险低，崩溃重启场景仍有
- 链接点击（OSC 8/URL）：**两侧都没有**，非迁移缺口，不追

### 已排除的伪缺口（审计纠错，别再当任务）

- ~~鼠标上报接线~~：已完成（`pane.rs` 的 `.on_input()` 覆盖 down/move/up/wheel，三模式三编码全通）
- ~~tab 拖拽排序~~ / ~~中键关闭 tab~~：原版 Tauri 也没有这两个功能
- ~~分屏比例跨重启回均分~~：G 收尾已修（首帧只量尺、下一帧铺分屏树）

## 性能观察（后续量化）

gpui-component 的 `ResizableState::update_panel_size` 每帧 `cx.notify()`，`ResizablePanel::render` 又 `state.read(cx)`——视图每帧被判失效可能持续重绘。三栏 + 分屏树全用 resizable，需实测 CPU/GPU；改不动上游就考虑自绘分隔条。

# T 批规格：系统托盘（audit #21）

> 2026-08-19 由规格提取 agent 逐文件对照产出（原任务「壳层三件」被拆分，本文件只含托盘节；
> 标题栏 #20 与壳层杂项 #30 的规格另行提取）。基线：src-tauri/src/tray.rs（373 行含 60 行单测）
> + src/store.ts 托盘段。所有行号以提取时源码为准，实现前回读核对。

## 1. 原版架构总述

**Rust 侧零业务逻辑**：裁剪、排序、emoji、i18n、tooltip 拼接、去重签名、seq 发号全在前端；
Rust 只存状态、画图标、建菜单、转发点击事件。迁到 GPUI 后这些**必须全部搬进 Rust**
（`collectAiProjects` 等价物 + `KIND_EMOJI` + `AI_PROJECT_KIND_ORDER` 需要新写）；
`seq` 乱序裁决机制可整个删掉（GPUI 单进程同步调用，无线程池乱序）。

实现是 macOS 菜单栏优先设计（NSStatusItem/18pt/retina 注释），Windows 靠 tauri 的 tray-icon 抹平。

## 2. 图标（代码逐像素画，无资源文件）

常量（tray.rs:31-45）：
- `DOT: u32 = 36`（2x 画布，显示 18pt）；`TRAY_ID = "status-light"`
- `BLINK_MS = 600`；`BURST_FRAMES = 6`（≈3.6s）；`DIM = 0.35`
- 四色（Apple 系统色板）：GRAY `#8E8E93` / BLUE `#0A84FF` / YELLOW `#FFCC00` / GREEN `#34C759`

语义（tray.rs:5-8）：黄=待确认（授权/输入请求，含 error）；蓝=处理中（ai-working 含 API 重试）；
绿=已完成未读；灰=全部安静。

绘制 `compose_frame(color, dim)`（tray.rs:89-117）：实心圆+1px 软边抗锯齿，无边框无描边。
`radius = DOT/2 - 5.0`（=13px，直径 26 居中 36 画布）；`cx = cy = 17.5`；
每像素 `alpha = (radius + 0.5 - dist).clamp(0,1)`，dim 时 `alpha *= 0.35`；RGBA 只写 alpha>0。

状态→颜色：`active_colors()`（tray.rs:73-85）按固定顺序 **黄→蓝→绿** 收集（灰不在集合，空=灰）。
`frame_color(colors, frame, blinking)`（tray.rs:122-134）：
- 0 色 → 灰不闪
- 1 色 → 偶帧亮/奇帧暗（呼吸）
- n 色 → blinking 时 `colors[frame % n]` 轮转；否则停在最高优先级色

闪烁三档（tray.rs:12-14, 219-241）：聚焦不闪 / 失焦多状态持续轮转 / 失焦单状态爆闪 6 帧后
`settled=true` 定格全亮（tray.rs:232-235，注释：「持续呼吸闪太抢注意力(用户反馈)」）。
闪烁线程 600ms sleep，条件 `enabled && n!=0 && !focused && !settled` 才推帧 →
`run_on_main_thread(redraw)`。相位只在灯色/焦点变化时重置 frame=0 settled=false（tray.rs:289-303）。

Builder（tray.rs:196-217）：初始灰、`icon_as_template(false)`（彩色语义防单色化）、
`show_menu_on_left_click(false)`。

## 3. 菜单

- **只有项目条目**：无「显示窗口」/「退出」/分隔符（tray.rs:174-183）。
- 项目 id `proj:{id}`；label 由前端拼好（含 emoji+i18n），Rust 零文案。
- 每次推送**整菜单重建**；`projects` 空 → `set_menu(None)`（无右键菜单）。
- 前端拼法（store.ts:330-334）：`KIND_EMOJI = { attention:🟡, working:🔵, done:🟢, idle:⚪ }`；
  label = `emoji + 空格 + 项目名 + " · " + t("app.trayStatus.{kind}")`。
- 排序（store.ts:265, 313）：`attention(0) > working(1) > done(2) > idle(3)`。
  与「点击跳转」优先级**有意不同**（跳转是 待确认 > 最先完成 > 处理中，attentionJump.ts:10-12）。
- 入选（store.ts:282-312）：项目下任一 pane 有 AI 会话（含 ai-idle）即入列，取项目内最高档。
  pane 级判据：`status=='error'||attention`→attention；`ai-working`→working；`ai-idle`→idle；
  `donePaneIds.has && status!='ai-working'`→done。
- **trayMaxProjects 裁剪全在前端**（store.ts:322, 331）：`?? 5`，`slice(0, max)`，超出直接不显示
  无省略提示；UI 限幅 min 1 / max 20（SettingsModal.tsx:1380-1381）。

## 4. 点击语义

- **左键**：Rust 无条件唤起窗口（不看开关）——`TrayIconEvent::Click` + `MouseButton::Left` →
  `focus_main_window`（show→unminimize→set_focus，窗口名 "main"，tray.rs:247-253）→
  `emit("tray-clicked")`。无双击分支，不区分 ButtonState。
  前端（App.tsx:303-306）：`trayClickFocus ?? true` 为 false 时收到事件什么都不做（窗口已被唤起，留在原地）；
  true 时 `focusAttentionTarget()`。
- **菜单点项目**（tray.rs:211-216 + App.tsx:309-315）：唤窗 + `emit("tray-project-clicked", id)`；
  前端校验项目仍在 → `if (!focusAttentionTarget(projectId)) setActiveProject(projectId)`
  （定位不到也要切项目，不能没反应）。**菜单路径不受 trayClickFocus 管辖**。
- 落点算法 `pickAttentionTarget`（attentionTarget.ts:24-53）：待确认/异常 > 已完成（aiDoneOrder
  seq 最小最先）> 处理中；全空闲 null。GPUI 侧已有同源实现 `notify::pick_attention_target`。

## 5. tooltip

前端拼（store.ts:339-348）：attention/working/done 三个 **pane 级**计数（ai-idle 不计入），
`t("app.trayAttention", {count})` 等三条，`parts.join(' · ')`。Rust 空串→`set_tooltip(None)`。

## 6. 数据流与推送时机

命令 `set_tray_status`（tray.rs:267-312）参数：`seq, attention, working, done, tooltip, projects, enabled, focused`。
事件（Rust→前端）：`tray-clicked` / `tray-project-clicked`。

前端 `syncTrayStatus`（store.ts:319-353）触发点（全 queueMicrotask）：
clearPaneAttentionByPty(:247) / 关闭项目(:864) / setProjectLayout(:928) /
updatePaneStatusByPty 末尾(:1099) / clearUnreadDone(:1108) /
窗口焦点变化（App.tsx:290-299，聚焦先 clearUnreadDone 再 sync）/ 托盘配置变化（App.tsx:318-322）。

去重签名（store.ts:336-338）：`enabled|focused|attention|working|done|labels.join(',')`，不含 tooltip（等价导出）。
`unreadDonePaneIds` 写入判据（store.ts:984）：`isDone && !attention && foundPaneId && !windowFocusedFlag`
——看**原生**窗口焦点（onFocusChanged 驱动模块级 flag，DOM focus 不可靠曾致绿灯不灭）。

初始状态：`enabled:true, focused:true`（启动即认为聚焦，避免开局失焦闪烁，tray.rs:188-194）。
初始化失败只 eprintln 不中断启动（lib.rs:70-77）。托盘完全程序化创建（tauri.conf.json 无 trayIcon 段）。
**无退出项、无关窗最小化到托盘**——保持原样即可。

## 7. config 字段（GPUI 侧已存在，零消费）

`tray_status_enabled: Option<bool>`（None=默认开）/ `tray_max_projects: Option<u32>`（None=5）/
`tray_click_focus: Option<bool>`（None=开）——crates/mt-config/src/config.rs:164-173 与
Tauri 侧逐字相同，Default 全 None。UI 兜底值：true / 5(1..20) / true。

## 8. i18n（好消息：7+7 条全部已搬）

- `app.trayStatus.{attention,working,done,idle}` = 待确认/处理中/已完成/AI 空闲（en: Awaiting confirmation/Working/Completed/AI idle）——dict.rs APP_ZH:52-58 / APP_EN:96-102，与 TS 逐字一致
- `app.trayAttention/trayWorking/trayDone` = `{count} 个待确认` 等三条插值
- 设置页 `settings.system.trayGroup/trayStatusTitle/trayStatusDesc/trayClickFocusTitle/trayClickFocusDesc/trayMaxTitle/trayMaxDesc` ——dict.rs:1077-1083(zh)/1264-1270(en)

## 9. GPUI 侧现状

- **托盘代码为零**；notify.rs:14-16 显式声明「托盘不做（当时交付范围排除），补上时不必改 unread 判据」。
- 现成消费口：
  - `store.rs:1087-1090 unread_done_count()` → DoneTracker::unread_count()（unread 集合看窗口焦点，
    聚焦时 clear_unread，store.rs:1076-1085）
  - `store.rs:1118-1135 next_attention_target(only_project)` → (project_id, pane_id)，
    委托 notify::pick_attention_target（notify.rs:198-230，与 TS 同源）
  - `is_pane_unread_done` / `clear_unread_done` / `global_ai_status`
- **缺 `collectAiProjects` 等价物**（按项目聚合 entries + 三计数）——需要新写。
- Win32 调用方式参考 notify.rs：用 `windows` crate 0.61 直调 unsafe，无封装。
  **HWND 获取模式必须复用 notify.rs:283-289**：`HasWindowHandle::window_handle(window)`（显式走 trait，
  gpui Window 有同名固有方法返回 AnyWindowHandle 的坑已记注释）→ `RawWindowHandle::Win32` →
  `HWND(win32.hwnd.get() as *mut c_void)`。

## 10. 依赖决议输入（需主会话动根/mt-app Cargo.toml 时参考）

- workspace 无 image/tray-icon/muda/winit；Cargo.lock 已有 `image 0.25.10`（gpui 间接）、
  `windows 0.61.3`（gpui 与 mt-app 共用）与 `windows 0.57.0`（仅 sysinfo）。
- **建议 Win32 直写 Shell_NotifyIconW**（不引 tray-icon：它拉 muda/全局事件循环钩子，与 gpui
  事件循环共存有风险且旧版是靠 tauri 集成的）。mt-app 的 `windows` 依赖加 feature：
  `Win32_UI_Shell`（Shell_NotifyIconW/NOTIFYICONDATAW）、`Win32_Graphics_Gdi`
  （CreateDIBSection/CreateIconIndirect 造 HICON）；继续用 0.61 不新增第四份 windows crate。
- **Windows 托盘图标必须是 HICON**：CreateDIBSection + CreateIconIndirect；
  **每次换图标要 DestroyIcon 旧的**，否则 GDI 句柄泄漏（tray-icon 以前帮忙做，自己写自己管）。
- 尺寸：36px 画布偏大，用 `GetSystemMetrics(SM_CXSMICON)`（16px@100% 按 DPI 缩放）；
  半径公式改比例式 `size * 0.361`（=13/36）等效。
- 托盘消息回调需要一个隐藏消息窗口（或挂主窗口 WndProc 子类化）收 WM_APP 回调与
  WM_CONTEXTMENU/WM_LBUTTONUP；右键菜单用 TrackPopupMenu（需 `Win32_UI_WindowsAndMessaging` 已有）。
  实现自选，但回调必须跳回 GPUI 主线程再动 store。

## 11. 实现要点清单（开发 agent 照此自检）

1. 灰/黄/蓝/绿四色 + 三档闪烁 + settled 定格语义逐条对齐（有单测钉 frame_color/active_colors 逻辑）
2. 菜单只列项目、上限裁剪、排序 attention>working>done>idle、emoji+名+状态文案格式一字不差
3. 左键：无条件唤窗 + trayClickFocus 门控跳 pane；菜单点项目：唤窗+定位失败切项目、不受开关管辖
4. tooltip 三计数 pane 级、ai-idle 不计
5. 推送时机换成 GPUI 侧等价钩子（状态变化/焦点变化/配置变化/项目与 pane 增删），带去重签名
6. enabled=false → 图标隐藏且全部逻辑早退；配置热更新即时生效
7. 图标 HICON 生命周期管理（DestroyIcon）；托盘进程退出时 Shell_NotifyIcon(NIM_DELETE)
8. 非 Windows：空实现（与 notify.rs 同模式）

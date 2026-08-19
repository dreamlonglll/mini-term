# 批规格：自定义标题栏 + 壳层杂项 + Toast/提示音（audit #20 / #30 / 其他细项）

> 2026-08-19 由规格提取 agent 逐文件对照产出。基线：`src/components/TitleBar.tsx`（351 行）、
> `src-tauri/src/window_snap.rs`（283 行）、`src/App.tsx`、`src/store.ts`、
> `src/components/ToastContainer.tsx`、`src/utils/notificationSound.ts` 等。
> 托盘（#21）已由 `docs/batch-specs/tray.md` 覆盖，本文件**不重复**——
> `collectAiProjects` 等价物与 HWND 获取模式两节直接引用 tray.md §9/§10。
> 所有行号以提取时源码为准，实现前回读核对。

---

## 目录

- [A. 自定义标题栏（#20）](#a-自定义标题栏20)
- [B. 壳层杂项（#30）](#b-壳层杂项30)
- [C. Toast 与提示音（其他细项）](#c-toast-与提示音其他细项)

---

# A. 自定义标题栏（#20）

## A.0 总述与挂载

| 项 | 值 | 出处 |
|---|---|---|
| 组件 | `TitleBar({ version })` | `src/components/TitleBar.tsx:101` |
| 挂载点 | `App.tsx` 根 flex-col 的**首个** child | `src/App.tsx:474-478` |
| **不受 `configLoaded` 门控** | 配置加载失败时用户也得有地方能关窗口 | `App.tsx:476-477`（原注释） |
| 高度 | `TITLE_BAR_HEIGHT = 32`（对齐 Windows 原生 32px，"窗口按钮的手感才对得上"） | `TitleBar.tsx:12-13` |
| macOS 交通灯占位 | `MAC_TRAFFIC_LIGHT_WIDTH = 78` 的空 div 顶在最左 | `TitleBar.tsx:15, 207` |
| 根容器样式 | `flex items-stretch shrink-0 select-none bg-[var(--bg-surface)] border-b border-[var(--border-subtle)]`，`data-titlebar` 属性 | `TitleBar.tsx:200-205` |
| version 来源 | `getVersion()` → `setCurrentVersion` + `setTitle('Mini-Term v{ver}')` | `App.tsx:273-281` |

`data-titlebar` 属性除自身外**无消费方**（全仓仅 `TitleBar.tsx:201` 一处），迁移可丢弃。

## A.1 拖拽区与 no-drag 区

**关键决策（必须照搬）**：不用 CSS `-webkit-app-region`。原注释（`TitleBar.tsx:185-186`）：
> `-webkit-app-region` 会让 WebView2 进模态循环、外部工具一介入就锁住输入（v0.2.16 修过一轮），拖拽一律走 Tauri API。

`handleMouseDown`（`TitleBar.tsx:182-193`）挂在**根容器**上，判定链：

1. `e.button !== 0` → 直接 return（只有左键拖窗）
2. `(e.target).closest('[data-no-drag]')` 命中 → return
3. `e.detail === 2`（双击）→ `appWindow.toggleMaximize()`
4. 否则 → `appWindow.startDragging()`

即：**整条标题栏默认都是拖拽区**，`data-no-drag` 是"挖洞"机制。挂 `data-no-drag` 的元素共 3 处：

| 元素 | 位置 |
|---|---|
| 项目切换胶囊的外层 `div`（含下拉） | `TitleBar.tsx:223` |
| 全局状态灯按钮 | `TitleBar.tsx:291` |
| 窗口控制三键的容器 | `TitleBar.tsx:315` |

中段 `flex-1` 空白（`TitleBar.tsx:311`）是主要拖拽区。

## A.2 窗口控制三按钮

统一 `buttonClass`（`TitleBar.tsx:195-197`）：

```
w-[46px] h-full flex items-center justify-center text-[var(--text-secondary)]
hover:bg-[var(--border-default)] hover:text-[var(--text-primary)] transition-colors
```

图标全部是 **10×10 viewBox / stroke=currentColor / strokeWidth=1** 的细线（`TitleBar.tsx:17-40`，
原注释："Windows 的画法是 10×10 内的细线，不是 Material 那种粗描边"）：

| 按钮 | 图形 path | i18n key | 行号 |
|---|---|---|---|
| 最小化 | `M0 5.5h10`（一条横线） | `app.titleBar.minimize` | `:19-23, 316-324` |
| 最大化 | `<rect x=0.5 y=0.5 w=9 h=9>` | `app.titleBar.maximize` | `:24-28` |
| 还原（已最大化态） | `<rect x=0.5 y=2.5 w=7 h=7>` + `M2.5 2.5V0.5h7v7h-2`（后层只画露出的两条边，"画成完整方框会糊成一团"） | `app.titleBar.restore` | `:29-35` |
| 关闭 | `M0.5 0.5l9 9M9.5 0.5l-9 9` | `app.titleBar.close` | `:36-40, 337-346` |

**两态切换**：`maximized` state（`:107`）由 effect 维护——`appWindow.isMaximized()` 首次同步 +
`appWindow.onResized(sync)` 订阅（`:135-149`），组件卸载置 `disposed` 防竞态 setState。

**关闭按钮特殊样式**（不复用 `buttonClass`，`:337-341`）：
`hover:bg-[#c42b1c] hover:text-white`（Windows 系统红字面量，**不是** CSS 变量）。

**关闭走 `close()` 而非 `destroy()`**（`:342-343` 原注释）：要走 `onCloseRequested`，
AI 会话确认与配置落盘都挂在那（见 §B.1）。

## A.3 Win11 贴靠（Snap Layouts）

前端只做一件事：把最大化按钮的矩形报给后端。

**上报 effect**（`TitleBar.tsx:158-180`）：
- 仅 `isWindows` 执行
- `maxButtonRef.getBoundingClientRect()` → `invoke('set_max_button_rect', {x, y, width, height})`（CSS 像素）
- `ResizeObserver` 同时观察**按钮自身**（字号/缩放变化）与 `document.documentElement`（窗口宽度决定按钮 x）
- **卸载时必须上报 `{0,0,0,0}` 撤销**（`:177-178` 原注释："否则残留一片点不动的死区"）

**后端 `window_snap.rs`**（关键行）：

| 项 | 值 / 行号 |
|---|---|
| 安装时机 | `install(&window)` 在窗口创建后调一次；`SetWindowSubclass(hwnd, proc, SUBCLASS_ID, 0)`，`SUBCLASS_ID = 0x4D54_5342`（"MTSB"） | `:38-39, 108-122` |
| 矩形存储 | 5 个 atomic（`RECT_SET` + 四条边）；**先写四条边再置 `RECT_SET`**，保证命中测试读到的是完整矩形 | `:45-49, 131-136` |
| 撤销语义 | `width<=0 \|\| height<=0` → `RECT_SET=false` + 清 hover | `:126-130` |
| `WM_NCHITTEST` | **先问 `DefSubclassProc`**，只有返回 `HTCLIENT`(1) 时才改判 `HTMAXBUTTON`(9)——否则窗口上沿几像素被按钮吃掉，纵向 resize 拉不动 | `:148-156`（注释在 `:149-150`） |
| DPI 换算 | 上报是 CSS 像素、命中测试是物理像素，按 `GetDpiForWindow/96.0` 缩放后比对 | `:206-214` |
| 屏幕坐标符号 | `lparam` 的 lo/hi word 必须按 **有符号** 取（副屏在主屏左侧时为负），有单测 | `:237-243, 249-255` |
| 悬停态回传 | `WM_NCMOUSEMOVE` 且 `wparam==HTMAXBUTTON` → `set_hovering(true)` + `TrackMouseEvent(TME_LEAVE\|TME_NONCLIENT)`（非客户区 leave 要显式订阅）；`WM_NCMOUSELEAVE` / `WM_MOUSEMOVE` 都补一刀 `false` | `:157-175, 217-225` |
| 事件去重 | `HOVERING.swap()` 变化才 `emit("titlebar-max-hover", bool)`——每次鼠标移动都跑，不去重会淹了前端 | `:227-235` |
| 按下吞掉 | `WM_NCLBUTTONDOWN` + `HTMAXBUTTON` → 返回 0 不交默认处理（"默认会进系统菜单的模态循环，把 WebView 的输入卡住"） | `:176-177` |
| 点击落地 | `WM_NCLBUTTONUP` + `HTMAXBUTTON` → `IsZoomed ? SC_RESTORE(0xF120) : SC_MAXIMIZE(0xF030)`，`PostMessageW(WM_SYSCOMMAND)` | `:178-187` |

前端消费悬停态：`useTauriEvent<boolean>('titlebar-max-hover')` → `ncMaxHover` state（`TitleBar.tsx:151-153`），
最大化按钮 class 追加 `bg-[var(--border-default)] !text-[var(--text-primary)]`（`:328`）。
按钮的 `onClick`（`:333`）在 Windows 上**永远收不到事件**，留着是给 Linux 与"命中测试没装上"的降级路径。

## A.4 项目切换胶囊

数据来源：`collectAiProjects(projectStates, projects, aiDoneOrder).entries`（`TitleBar.tsx:119`）。
**注意 done 判据用 `aiDoneOrder`（不看窗口焦点）而不是托盘用的 `unreadDonePaneIds`**——
与旁边的全局状态灯同一套语义（`:118` 原注释；`store.ts:269-270` 也写了这条）。
`collectAiProjects` 全文见 `store.ts:273-315`，规格已在 tray.md §3「入选/排序」段落逐条列出，此处不重复。

`activeKind = aiProjects.find(p => p.id === activeProjectId)?.kind`（`:121`），
`undefined` = 当前项目没有 AI 会话。

**触发按钮**（`:225-248`，`title = t('app.titleBar.projectSwitcher')`）：

```
flex items-center gap-1.5 max-w-[220px] h-[22px] pl-2 pr-1.5 rounded-full
border border-[var(--border-default)] bg-[var(--bg-elevated)] text-xs text-[var(--text-primary)]
hover:border-[var(--accent)] hover:bg-[var(--border-subtle)] transition-colors
```

- 前面有一条竖分隔线：`mx-1 w-px h-3.5 bg-[var(--border-default)]`（`:222`；原注释：纯文字紧挨版本号会被误读成标题的一部分）
- 状态点：`w-1.5 h-1.5 rounded-full`（6px），色 = `LIGHT_COLORS[activeKind ?? 'idle']`，
  `opacity: activeKind ? 1 : 0.45`（`:231-237`）
- 项目名：`truncate`
- 箭头：`ICON_CHEVRON_DOWN`（9×9，viewBox 10，strokeWidth 1.2，path `M1.5 3.25L5 6.75l3.5-3.5`，`:50-54`），
  展开时 `transform: rotate(180deg)`，`transition-transform duration-150`（`:239-247`）
- **`activeProject` 为空（没有项目）时整块胶囊 + 分隔线都不渲染**（`:220`）

**下拉面板**（`:250-282`）：

```
absolute left-0 top-full z-50 mt-1.5 min-w-[220px] max-w-[320px]
bg-[var(--bg-elevated)] border border-[var(--border-default)]
rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] overflow-hidden
```

- 空列表 → 一行 `px-3 py-2 text-xs text-[var(--text-muted)]`，文案 `app.titleBar.noAiProjects`
- 每行：`flex items-center gap-2 px-3 py-1.5 text-xs cursor-pointer transition-colors duration-100`
  - 当前项目行：`bg-[var(--accent-subtle)] text-[var(--accent)]`
  - 其余：`text-[var(--text-primary)] hover:bg-[var(--border-subtle)]`
  - 左：6px 状态点（`LIGHT_COLORS[p.kind]`，**不压暗**）
  - 中：项目名 `truncate flex-1`
  - 右：`t('app.trayStatus.{p.kind}')`，`text-[var(--text-muted)] shrink-0`
- **点击语义**（`:264-270`，与托盘菜单同一套）：先关下拉，再
  `if (!focusAttentionTarget(p.id)) setActiveProject(p.id)`——定位不到目标（pane 已安静）也要把项目切过去
- **不列上限、不裁剪**（与托盘的 `trayMaxProjects` 不同），靠 `max-w-[320px]` + `truncate` 处理溢出
- 关闭：`document.mousedown` 监听，`switcherRef.current.contains(e.target)` 为假即关（`:124-133`）——
  没有 Esc 关闭、没有键盘导航（那是 `ProjectSwitcher.tsx` / audit #26 的事）

## A.5 全局状态灯

档位与颜色（`TitleBar.tsx:56-73`）：

| kind | 颜色变量 | 优先级 |
|---|---|---|
| `error` | `--color-error` | 4 |
| `attention` | `--color-warning` | 3 |
| `working` | `--color-ai-working` | 2 |
| `done` | `--color-success` | 1 |
| `idle` | `--text-muted` | 0 |

`computeLight`（`:76-94`）遍历**所有项目所有 pane**，取最紧急一档，判据顺序（if/else 链，先中先算）：
`status==='error'` → error；`pane.attention` → attention；`status==='ai-working'` → working；
`aiDoneOrder.has(pane.id)` → done。

> ⚠️ 与边条徽标（`ActivityBar.tsx` 的 `globalStatus`，GPUI 已实现为 `store.rs:1095 global_ai_status()`）
> **口径不同**：边条把 `error` 压成 `idle`，标题栏灯**保留 error 且列为最高档**。
> 两处不可互相复用，GPUI 侧需要新写一份 `title_bar_light()`。

按钮（`:290-307`）：`self-stretch px-1.5 flex items-center justify-center group`，
`title` / `aria-label` 都是 `t('app.titleBar.status.{light}')`（五条 key 全在字典）。
灯本体：`w-2 h-2 rounded-full transition-transform group-hover:scale-125`，
`working` 档追加 `animate-blink`（`styles.css:227-238`：`alertBlink 0.8s ease-in-out infinite`，
`0%,100% {opacity:1; scale(1)}` / `50% {opacity:0.2; scale(0.75)}`），
`idle` 档 `opacity: 0.45`。点击 → `focusAttentionTarget()`（全局，不限项目）。

## A.6 i18n 清单（全部已在 mt-i18n）

`crates/mt-i18n/src/dict.rs` app 命名空间 `:41-51`（zh）/ `:85-95`（en）：

`titleBar.close` / `titleBar.maximize` / `titleBar.minimize` / `titleBar.restore` /
`titleBar.projectSwitcher` / `titleBar.noAiProjects` /
`titleBar.status.{error,attention,working,done,idle}`

外加下拉右侧标签复用 `app.trayStatus.{attention,working,done,idle}`（dict.rs `:52-58` / `:96-102`）。
**标题栏这一节零缺词条。**

## A.7 GPUI 侧可行性调查

### A.7.1 现有窗口装配

`crates/mt-app/src/main.rs:1047-1057`：

```rust
let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
cx.open_window(WindowOptions {
    window_bounds: Some(WindowBounds::Windowed(bounds)),
    titlebar: Some(TitlebarOptions {
        title: Some(format!("Mini-Term v{}", env!("CARGO_PKG_VERSION")).into()),
        ..Default::default()          // ← appears_transparent 默认 false = 系统原生标题栏
    }),
    ..Default::default()
}, ...)
```

即**当前跑的是 Windows 原生标题栏**，窗口尺寸不持久化（原版也不持久化，只存三栏宽度）。

### A.7.2 gpui 0.2.2 的确切 API（读 `~/.cargo/registry/.../gpui-0.2.2` 源码）

**`WindowOptions`**（`src/platform.rs:1088-1133`）字段全表：
`window_bounds` / `titlebar` / `focus` / `show` / `kind` / `is_movable` / `is_resizable` /
`is_minimizable` / `display_id` / `window_background` / `app_id` / `window_min_size` /
`window_decorations` / `tabbing_identifier`。

**`TitlebarOptions`**（`src/platform.rs:1246-1256`）只有三个字段：

```rust
pub struct TitlebarOptions {
    pub title: Option<SharedString>,
    /// Should the default system titlebar be hidden to allow for a custom-drawn titlebar?
    /// (macOS and Windows only)
    pub appears_transparent: bool,
    /// macOS 交通灯位置
    pub traffic_light_position: Option<Point<Pixels>>,
}
```

- **`appears_transparent: true` 就是"无边框/自绘标题栏"开关**，Windows 与 macOS 都认。
  Windows 侧映射成 `hide_title_bar`（`platform/windows/window.rs:380`），
  进而驱动 `WM_NCCALCSIZE`（`events.rs:705` 起，吃掉系统 caption 高度）与 `WM_NCHITTEST`。
- `window_decorations: Option<WindowDecorations>`（`Client` / `Server`）**只在 Wayland 生效**
  （字段注释原文 "Wayland only / Note that this may be ignored"）；X11 实现见
  `platform/linux/x11/window.rs:1528`，Windows/macOS 平台实现里没有它。
  `Window::window_decorations() -> Decorations`（`window.rs:1769`）在 Windows 上恒返回 `Server`。
  → **Windows 下不要碰 `window_decorations`，`appears_transparent` 才是那个开关。**
- 没有 `client_side_decorations` 这个公开 API；`x11/client.rs:187` 的
  `client_side_decorations_supported` 是私有字段。

**`WindowControlArea`**（`src/window.rs:477-489`）——这是 Win11 贴靠的**官方通道**：

```rust
pub enum WindowControlArea { Drag, Close, Max, Min }
```

用法（`elements/div.rs:1004`，`InteractiveElement` trait 方法）：

```rust
div().window_control_area(WindowControlArea::Max)   // 元素级，paint 时登记 hitbox
```

底层：`Window::insert_window_control_hitbox(area, hitbox)`（`window.rs:3324`）→
`next_frame.window_control_hitboxes` → Windows 的 `handle_hit_test_msg`
（`platform/windows/events.rs:855-880`）把命中结果直接翻成
`Drag→HTCAPTION` / `Close→HTCLOSE` / `Max→HTMAXBUTTON` / `Min→HTMINBUTTON`。

**这意味着 `window_snap.rs` 那 283 行可以整个删掉**：矩形上报、DPI 换算、
子类化、`TrackMouseEvent`、悬停事件回传全部由 gpui 内建。

**点击与双击也是白拿的**：
- `handle_nc_mouse_down_msg`（`events.rs:985-996`）：左键按在 HTMIN/HTMAX/HTCLOSE 上记 `nc_button_pressed` 并吞掉默认处理（同样规避系统菜单模态循环）
- `handle_nc_mouse_up_msg`（`events.rs:1032-1058`）：按下/抬起同一区域才动作 ——
  `Min→ShowWindowAsync(SW_MINIMIZE)`；`Max→is_maximized() ? SW_NORMAL : SW_MAXIMIZE`；
  `Close→PostMessage(WM_CLOSE)`（**会走 `on_window_should_close`**，正好接 §B.1）
- `WM_NCLBUTTONDBLCLK` 与 `WM_NCLBUTTONDOWN` 走同一分支（`events.rs:63-65`），
  注释原文："If you don't interact with any elements, this will fall through to the
  windows default behavior of toggling whether the window is maximized" ——
  **双击 Drag 区最大化/还原是系统行为，不用自己写**
- resize 边框：`handle_hit_test_msg`（`events.rs:886-919`）在 `hide_title_bar` 下先问
  `DefWindowProcW` 取 8 个边角，再对未最大化窗口的顶部 `SM_CYFRAME` 像素返回 `HTTOP`
  —— 与 `window_snap.rs:149-150` 那条"先问原 proc"的教训同源，gpui 已经踩过

**悬停态**：`handle_nc_mouse_move_msg`（`events.rs:921-946`）把非客户区鼠标移动**照常翻译成
`PlatformInput::MouseMove` 喂进 gpui**，因此 `.hover(...)` 样式在 HTMAXBUTTON 区域**正常生效** ——
不需要 `titlebar-max-hover` 那条回传事件（WebView2 时代的补丁在 GPUI 下自然消失）。

**其它相关 Window API**：

| API | 位置 | 用途 |
|---|---|---|
| `window.is_maximized() -> bool` | `window.rs:1450` | 最大化/还原图标两态 |
| `window.zoom_window()` | `window.rs:1741` | 降级路径 / Linux 点击 |
| `window.minimize_window()` | `window.rs:4117` | 同上 |
| `window.start_window_move()` | `window.rs:1753` | Linux 拖窗（Windows 靠 HTCAPTION） |
| `window.titlebar_double_click()` | `window.rs:4383` | macOS 双击标题栏（跟随系统设置） |
| `window.is_window_active()` | `window.rs:1721` | 失焦时标题栏压暗（原版没做，可不做） |
| `window.remove_window()` | `window.rs:1375` | 关窗（**绕过** should_close，慎用） |
| `window.set_window_title(&str)` | `window.rs:1779` | 任务栏/Alt+Tab 标题 |

### A.7.3 gpui-component 的 `TitleBar` 能不能直接用

`gpui-component-0.5.1/src/title_bar.rs`（329 行）提供 `TitleBar` + `ControlIcon` +
`TitleBar::title_bar_options()`（返回 `appears_transparent: true` + 交通灯位置 `(9,9)`）。

**结论：`title_bar_options()` 可以抄（或直接调），`TitleBar` 元素本身不能用**，四条硬伤与
M 批边条、P 批菜单同源：

1. **图标全是 `IconName::WindowMinimize/WindowRestore/WindowMaximize/WindowClose`**
   （`title_bar.rs:102-109`），走 `Icon::path()` → `"icons/window-close.svg"`（`icon.rs:122+`），
   而本仓 `Application` 从未注册 `AssetSource`（`grep with_assets` 零命中，
   `activity_bar.rs:7` / `menu.rs:10` 已记同一坑）→ **渲染空白且编译期无感**
2. 高度写死 `TITLE_BAR_HEIGHT = px(34.)`（`title_bar.rs:14`），按钮宽度 = 高度（34px），
   原版是 32px / 46px 宽
3. 配色走 `cx.theme().title_bar` / `title_bar_border` / `secondary_hover` / `danger`
   （gpui-component 自己的 theme token），与壳层 `ui::palette()` 对不上
4. 布局是 `justify_between` 的两段式（children 一段 + WindowControls 一段），
   塞不下"品牌 / 版本 / 胶囊 / 状态灯 / 中段拖拽空白"这套五段结构

**可抄的两点**：① `.window_control_area(...)` 只在 `cfg!(windows)` 下挂（`title_bar.rs:176-178`），
Linux 走 `on_click` + `window.minimize_window()/zoom_window()/remove_window()`（`:179-198`）；
② 整条 bar 的 `h_flex().window_control_area(WindowControlArea::Drag)`（`:303`）——
Drag 区用**一个覆盖中段的元素**声明，而不是给根容器加、再给子元素挖洞。

### A.7.4 建议方案

**自绘，不用 gpui-component 的 TitleBar。** 新增 `crates/mt-app/src/title_bar.rs`：

1. **窗口装配**：`main.rs` 的 `TitlebarOptions` 加 `appears_transparent: true`，
   `traffic_light_position: Some(point(px(9.), px(9.)))`（macOS 用；本仓主力 Windows，留着不亏）。
   `title` 保留现有 `Mini-Term v{ver}`（任务栏预览与 Alt+Tab 仍读它）。
2. **结构**（h_flex，`h(px(32.))`，`bg(ui::bg_surface())`，`border_b_1 border_color(ui::border_subtle())`）：
   - macOS 占位 `div().w(px(78.))`（`cfg!(target_os="macos")`）
   - 品牌段：logo（`mt_ui::icons::VectorIcon` 自绘，照抄 `ICON_LOGO` 的 rect+path，
     `TitleBar.tsx:42-47`）+ `Mini-Term` + `v{CARGO_PKG_VERSION}`
   - 竖分隔线 + 项目切换胶囊（用 `gpui_component::popover` 或自建 deferred 浮层；
     **P 批已有的 `menu.rs` 基建可直接复用**——它已解决 deferred/anchored/全窗遮罩点外关/贴边收拢）
   - 全局状态灯按钮（`StatusDot` 不合适：那是四态勾叉字形；这里要**纯色圆点**，
     直接 `div().w(px(8.)).h(px(8.)).rounded_full().bg(color)`）
   - `div().flex_1().window_control_area(WindowControlArea::Drag)` ← **中段拖拽区**
   - 三按钮 h_flex，每颗 `w(px(46.)).h_full()` + `.window_control_area(Min/Max/Close)`
3. **拖拽区做"正列"而不是"挖洞"**：只给中段空白 + 品牌段挂 `Drag`，
   胶囊/状态灯/三键**不挂**——这样天然等价于 `data-no-drag`，且不必模拟 `closest()` 语义。
   ⚠️ 品牌段挂 Drag 后其内部不能再放可点元素（原版品牌段确实纯展示）。
4. **图标**：四个窗口控制图形用 `mt_ui::icons::vector` 的 `Shape` DSL 逐点照抄
   10×10 path（M 批边条、P 批菜单同法），线宽 1，颜色 `ui::text_secondary()`，
   hover `ui::text_primary()` + `bg(ui::border_default())`；关闭键 hover
   `bg(hsla from #c42b1c)` + 白字（字面量色，不进 palette——它是 Windows 系统红）。
5. **最大化两态**：`window.is_maximized()` 在 `render` 里直接读，无需订阅 resize
   （gpui 每帧重绘，天然同步；原版那套 `onResized` 订阅可整个删掉）。
6. **关闭键**：不自己挂 `on_click`——`window_control_area(Close)` 会让系统投 `WM_CLOSE`，
   gpui 转 `on_should_close` 回调（§B.1 的确认框挂在那）。**这一条是关键**：
   自己写 `on_click → remove_window()` 会绕过确认框。

### A.7.5 风险清单

| # | 风险 | 说明 / 缓解 |
|---|---|---|
| 1 | `window_control_area` 是 **paint 阶段**登记的 hitbox，首帧之前系统拿不到 | 首帧渲染完成前贴靠菜单不弹（几十毫秒，用户感知不到）。原版是 effect 上报，同样有这个窗口 |
| 2 | Drag 区吞掉右键 | Windows 上 HTCAPTION 区域右键会弹**系统窗口菜单**（移动/大小/关闭）。原版是 WebView 客户区，不会弹。行为差异，但更"像原生"，建议接受 |
| 3 | `appears_transparent` 后窗口**圆角与阴影**由系统 DWM 管 | Win11 上正常；Win10 会变直角。原版（Tauri decorations:false）同样如此，不是回归 |
| 4 | 顶部 `SM_CYFRAME` 像素被判 `HTTOP`（resize） | 未最大化时标题栏最上沿 ~4px 点不到按钮/胶囊。gpui 内建行为（`events.rs:914-917`），与原版 `window_snap.rs:149-150` 的取舍一致 |
| 5 | macOS 交通灯与自绘按钮**两套并存** | 必须 `cfg!(target_os="macos")` 时不渲染三按钮（原版 `TitleBar.tsx:314` 同款判定） |
| 6 | Linux 无 `window_control_area` 支持 | `x11/window.rs:1466` / `wayland/window.rs:1013` 的 `on_hit_test_window_control` 都是空实现 → Linux 必须走 `on_click` + `minimize_window/zoom_window/remove_window`（照抄 gpui-component 的 `is_linux` 分支）。平台支持现状：Linux 仅"代码支持"，可只留降级路径不验证 |
| 7 | 胶囊下拉的焦点 | P 批菜单基建的教训：浮层开时要收走终端焦点、关时先还回去再跑动作。切项目会重建终端视图，顺序错了会抢光标 |

## A.8 GPUI 现状差异清单（#20）

| # | 项 | 现状 | 缺口 |
|---|---|---|---|
| 1 | 自绘标题栏 | ❌ 用系统原生标题栏（`appears_transparent` 未开） | 整块新建 |
| 2 | 三按钮 | ❌ 系统画 | 自绘 + `window_control_area` |
| 3 | Win11 贴靠 | ✅ 系统原生标题栏天然有；改自绘后由 `WindowControlArea::Max` 接管 | 无需移植 `window_snap.rs`（283 行整个作废） |
| 4 | 悬停态回传 | — | WebView2 专属补丁，GPUI 下不存在 |
| 5 | 品牌 + 版本 | 🟡 版本已进窗口标题（`main.rs:1054`） | 标题栏内可视区未做 |
| 6 | 项目切换胶囊 | ❌ | 需 `collectAiProjects` 等价物（tray.md §9 记为共同缺口，**两批共用一份**） |
| 7 | 全局状态灯 | 🟡 边条有徽标（`global_ai_status()`，error 压 idle） | 标题栏灯口径不同（error 最高档 + done 档），需另写 |
| 8 | `focusAttentionTarget()` 等价物 | ✅ `store.next_attention_target()` + `JumpAttention` action（`main.rs:979`） | 直接复用 |
| 9 | i18n | ✅ 12 条 key 全在 | 无 |

---

# B. 壳层杂项（#30）

## B.1 关窗确认（盘点活 AI 会话列名）

**盘点函数** `collectLiveAiPanes()`（`src/App.tsx:52-66`）：

```
for (projectId, ps) of projectStates:
  if !ps.layout: continue
  projectName = config.projects.find(p => p.id === projectId)?.name ?? ''
  for pane of collectPanes(ps.layout):
    if pane.ptyId === undefined: continue          // ← 没起过进程的 pane 不算
    if pane.status !== 'ai-working' && !== 'ai-idle': continue
    label = pane.customTitle || pane.shellName
    names.push(projectName ? `· ${projectName} / ${label}` : `· ${label}`)
return { count: names.length, names }
```

注释（`:50-51`）："只数 AI 会话——裸 shell 关掉不心疼，AI 会话被 kill 才是真损失。"

**拦截钩子**（`App.tsx:392-422`）：

```
appWindow.onCloseRequested(async event => {
  event.preventDefault();
  live = collectLiveAiPanes();
  if (live.count > 0) {
    confirmed = await ask(
      t('app.closeConfirm.messageWithSessions', { count, names: names.join('\n') }),
      { title: t('app.closeConfirm.titleAi'), kind: 'warning' });
    if (!confirmed) return;                        // ← 保持窗口
  }
  for (projectId of projectStates.keys()) { flushLayoutToConfig(pid); flushExpandedDirsToConfig(pid); }
  if (currentActive && config.lastActiveProjectId !== currentActive) setConfig({...,lastActiveProjectId})
  await persistConfig().catch(() => {});           // flush 只改 store，最后统一写一次盘
  appWindow.destroy();
})
```

**设计意图**（`:389-391` 原注释，务必保留）：
> 只在真的会毁掉什么时才拦一下。之前无条件弹确认，日常开关十几次全是噪音，用户学会的是「闭眼点确定」——那正好让确认框在唯一该起作用的时候（AI 正在跑）也失效。

**i18n**：`app.closeConfirm.titleAi` / `app.closeConfirm.messageWithSessions`
（dict.rs `:30-31` zh / `:74-75` en，插值 `{count}` `{names}`，中文原文
"还有 {count} 个 AI 会话，关闭后它们会被终止：\n\n{names}\n\n确定退出吗？"）。

### 与 GPUI 侧 N 批已做的关 tab/pane 确认何异

| 维度 | 关窗（本项，未做） | 关 tab / 关整组（`pane_actions.rs`，已做） |
|---|---|---|
| 盘点范围 | **全部项目**的所有 pane | 单个 pane / 单个叶子内的 panes |
| `ptyId` 要求 | **要求 `ptyId !== undefined`**（`App.tsx:59`） | 不要求（`is_ai_alive` 只看 status，`pane_actions.rs:32-34`） |
| 状态判据 | `ai-working \|\| ai-idle` | **完全相同** |
| 无 AI 时 | **不弹**，直接关 | **照弹**（原版关 tab 也总是问，`pane_actions.rs:58`） |
| 名字格式 | `· {项目名} / {label}`，`\n` 拼进正文 | `label` 列表进 `Confirm::detail()` 灰色补充行 |
| label 取值 | `customTitle \|\| shellName` | `PaneState::label()`（同语义） |
| 文案 | `app.closeConfirm.*` | `paneGroup.closeAiTitle` / `closeTabAiMessage` / `closeGroupAiMessage` |

→ **可复用**：`is_ai_alive()`（`pane_actions.rs:32`）、`Confirm`（`prompt.rs:154-200`，
支持 `.detail(Vec<String>)` + `open_guarded` 防叠开）。
→ **要新写**：跨项目盘点 + `ptyId` 存在性判据 + `· 项目名 / 标签` 拼串。

### GPUI 关窗钩子现状

- **`window.on_window_should_close` 未注册**（全仓零命中）。签名（`gpui-0.2.2/src/window.rs:4329-4338`）：
  ```rust
  window.on_window_should_close(cx, move |window, cx| -> bool { /* false = 阻止关闭 */ })
  ```
  Windows 实现：`WM_CLOSE → handle_close_msg`（`events.rs:55, 256-260`），
  回调返回 `false` 就吞掉消息。
- 现有的只有 `cx.on_app_quit`（`main.rs:1040-1045`）：`save_config_now()` + `ai_bridge.shutdown()`。
- **⚠️ 异步陷阱**：`on_should_close` 要**同步**返回 bool，而 `Confirm` 是异步（用户点按钮才有结果）。
  实现套路：回调里先判"有没有活 AI"，无 → 返回 `true`；有 → 弹 `Confirm`、返回 `false`（先不关），
  确认回调里置一个 `force_close: bool` 标志再 `window.remove_window()`（它绕过 should_close）。
  **`remove_window()` 绕过钩子这点必须确认**（`window.rs:1375`），否则会死循环弹框。
- 配置落盘：`store.save_config_now()`（`store.rs:1480`）已存在，等价于 `persistConfig()`。
  `flushLayoutToConfig` / `flushExpandedDirsToConfig` 在 GPUI 侧是 `save_config_soon` 的防抖写入
  （`store.rs:1460`），关窗前应改调 `save_config_now()` 强刷。

## B.2 启动时自动版本检查与更新提醒 UI

> 设置页「关于」的手动检查按钮已由 `docs/batch-specs/settings-pages.md` 覆盖，此处只写
> **启动自动检查 + 边条提醒 UI**。

**`src/utils/updateChecker.ts`（全文 31 行）**：

| 项 | 值 |
|---|---|
| 仓库常量 | `GITHUB_REPO = 'dreamlonglll/mini-term'`（`:3`） |
| 接口 | `GET https://api.github.com/repos/{repo}/releases/latest`（`:22`，无 token、无 UA 头、无超时） |
| 失败文案 | 404 → `t('updateChecker.noRelease')`；其它 → `t('updateChecker.requestFailed', {status})`（`:23`） |
| 取值 | `version = data.tag_name` / `url = data.html_url` / `publishedAt = data.published_at`（`:25-29`） |
| 返回 | `compareVersions(release.version, current) > 0 ? release : null`（`:30`） |
| `compareVersions`（`:11-19`） | 去 `^v` 前缀 → `split('.').map(Number)` → 逐段相减，缺位补 0；**不处理 `-beta` 等预发布后缀**（`Number('1-beta')` = NaN，NaN 比较恒 false → 判为相等，退化成"不提示"） |

**调用时机**（`App.tsx:273-281`）：`getVersion()` 成功后**立即**跑一次，
`.catch(() => {})` 静默失败（离线不打扰）。全程只查一次，**无定时复查、无缓存、无"忽略此版本"**。

**提醒 UI**（`ActivityBar.tsx:173-182`）：有新版本才渲染这颗按钮（在 SSH/移动端之后、边条最下）：

```
relative w-8 h-8 flex items-center justify-center rounded text-[var(--accent)]
hover:bg-[var(--accent)]/15 transition-colors
title = t('app.update.title', { version })
```

- 图标 `ICON_UPDATE`（`ActivityBar.tsx:60-65`）：18×18 / viewBox 16 / strokeWidth 1.2，
  path `M8 10.5V3M5 6l3-3 3 3` + `M3 12.5h10`（上箭头 + 底横线）
- 红点：`absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full bg-[var(--accent)]
  border border-[var(--bg-surface)] animate-blink`
- 点击 → `openUrl(updateInfo.url)`（`App.tsx:488`，`@tauri-apps/plugin-opener`）

**i18n**：`app.update.title`（dict.rs `:60` / `:104`，插值 `{version}`）已在。
`app.update.badge`（`:59` / `:103`）**是死词条**——全仓零消费，迁移时不必接线（但也别删，
`mt-i18n` 的一致性测试按 TS 源头对账）。
`updateChecker.noRelease` / `requestFailed` 已在 dict.rs `:1516-1522`。

**GPUI 现状 / 实现要点**：
- ❌ 完全没做；`mt-app` **没有任何 HTTP 客户端依赖**（Cargo.toml 只有 gpui/组件库/mt-*/futures/chrono/serde_json/iana-time-zone）
- 依赖选型：`Cargo.lock` 已有 `rustls 0.23`（mt-relay，ring 后端）、`hyper`、`tokio`。
  建议 **`reqwest { default-features = false, features = ["rustls-tls", "json"] }`** ——
  复用已在树里的 rustls+hyper，不引第二套 TLS。
  ⚠️ 必须显式关 `default-features`，否则拉进 `native-tls`/OpenSSL（Windows MSVC 上的老坑，
  见 `spec/backend/rust-crypto-on-windows-msvc.md`）。
  替代方案 `ureq`（同步、体积小）——但它自带 rustls 版本，可能与 mt-relay 的分叉。
- 边条按钮位置：GPUI 边条已是 44px 图标栏（M 批），把这颗加在 `main.rs` 边条的
  `unread > 0` 跳转钮之后即可；`ICON_UPDATE` 用 `VectorIcon` 照抄两条 path。
- 打开浏览器：Windows 无 `plugin-opener` 等价物，用 `ShellExecuteW` /
  `std::process::Command::new("cmd").args(["/C","start","",url])`（注意空格路径与 `&` 转义；
  P 批 `reveal` 已踩过 `raw_arg` 那个坑，可参考）。
- 红点闪烁：`animate-blink` 在 GPUI 里用 `with_animation`（`mt-ui/src/icons/status.rs:278` 有范式，
  ⚠️ id 必须逐处唯一且稳定）。

## B.3 FirstRunGuide 完整版

`src/components/FirstRunGuide.tsx`（73 行）。渲染条件：`config.projects.length === 0`
（`App.tsx:534`，挂在终端栏位置）。

**布局**：`h-full bg-[var(--bg-terminal)] flex flex-col items-center justify-center gap-6 px-8 text-center`

| 段 | 内容 | i18n key | 行号 |
|---|---|---|---|
| 标题 | `text-base font-medium text-[var(--text-primary)]` | `app.firstRun.title`（"还没有项目"） | `:44-47` |
| 副标题 | `text-sm text-[var(--text-muted)]` | `app.firstRun.subtitle` | `:48` |
| 主按钮 | `px-4 py-2.5 rounded-[var(--radius-md)] text-sm border transition-all duration-200 border-[var(--accent)] bg-[var(--accent-subtle)] text-[var(--accent)] hover:bg-[var(--accent-muted)]` | `app.firstRun.addLocal` | `:29-31, 52-54` |
| 次按钮 | 同尺寸 + `border-dashed border-[var(--border-default)] text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)]` | `app.firstRun.addRemote` | `:32-34, 55-57` |
| 键位提示区 | `text-xs text-[var(--text-muted)] space-y-1.5 pt-2`，标题行 `opacity-70` | `app.firstRun.hintsTitle` | `:60-68` |

**两个入口的动作**：
- 本地（`:20-27`）：`open({directory:true, multiple:false})` → 取路径 →
  `name = path.split(/[/\\]/).pop() || path` → `addProject({id:genId(), name, path})` → `saveConfigToDisk()`
- 远程（`:55, 70`）：开 `AddRemoteProjectModal`（**属 SSH 批 audit #28**，本批依赖它；
  未就绪时可先隐藏这颗按钮，别渲染一个点不动的钮）

**三条键位提示**（`:36-40`，用 `hotkeyLabel(id)` 取显示串，`<kbd className="kbd">` 渲染）：

| id | 组合 | 描述 key |
|---|---|---|
| `newTerminal` | `Ctrl+Shift+T` | `settings.shortcuts.newTerminal` |
| `switchProject` | `Ctrl+Shift+P` | `settings.shortcuts.switchProject` |
| `terminalSearch` | `Ctrl+F` | `settings.shortcuts.terminalSearch` |

`comboLabel`（`src/utils/hotkeys.ts:125-134`）：`Ctrl/⌘` → `Shift/⇧` → `Alt/⌥` → 键名，
`code` 去 `^Key` 前缀，箭头键映射 `↑↓←→`，非 mac 用 `+` 连接、mac 直接拼接。

`.kbd` 样式（`styles.css:494-506`）：`inline-block; padding:1px 5px; radius:var(--radius-sm);
bg:var(--bg-elevated); border:1px solid var(--border-default); border-bottom-width:2px;
font-family:var(--app-font-mono); font-size:0.85em; line-height:1.5;
color:var(--text-secondary); white-space:nowrap`。

**GPUI 现状对照**：
- 现在只有一行文字：`terminal_area.rs:703-713`，无项目时渲染
  `t("app","emptyState")`（"请先在中间栏添加项目"）居中一行 —— 正是原版被吐槽的那个旧版本
- "项目有了但没终端"的空态（`terminal_area.rs:718-755`）已经做得不错：
  `terminalArea.emptyTitle` + `+ 新建终端 (Ctrl+Shift+T)` 按钮 —— 可作为样式参考
- i18n：`app.firstRun.{title,subtitle,addLocal,addRemote,hintsTitle}` 5 条 + 三条
  `settings.shortcuts.*` **全部已在** dict.rs（`:34-38` / `:1060,1069,1071`）
- 缺"快捷键显示串"的 Rust 侧等价物：GPUI 的键位定义散在 `main.rs:946-990` 的
  `KeyBinding::new("ctrl-shift-t", ...)` 字符串里。建议**硬编码三条显示串**
  （`"Ctrl+Shift+T"` 等）而不是造一套 `comboLabel` ——设置页快捷键表是另一批的事

## B.4 长文本粘贴转文件

**4 个配置字段**（`src/types.ts:42-44,47`；GPUI 侧 `mt-config/src/config.rs:137-147` **已全部存在且零消费**）：

| 字段 | Rust 名 | 默认 | 语义 |
|---|---|---|---|
| `longPasteToFile` | `long_paste_to_file` | `true` | 总开关 |
| `longPasteLineThreshold` | `long_paste_line_threshold` | `10` | 行数阈值，**0 = 不按行数判断** |
| `longPasteCharThreshold` | `long_paste_char_threshold` | `2000` | 字符阈值，**0 = 不按字符判断** |
| `remotePasteDir` | `remote_paste_dir` | `.mini-term/pasted` | SSH 远程项目的上传目录 |

**阈值判定** `isLongText`（`src/utils/terminalCache.ts:671-678`）：

```ts
if (charThreshold > 0 && text.length >= charThreshold) return true;   // ≥ 不是 >
if (lineThreshold > 0) {
  const lines = text.replace(/\r\n/g, '\n').split('\n').length;       // CRLF 归一后按 \n 切
  if (lines >= lineThreshold) return true;
}
return false;
```

**任一阈值命中即触发**（i18n footer 文案原话）。`text.length` 是 **UTF-16 code unit 数**
（JS `String.length`）——Rust 侧用 `chars().count()` 会与中文/emoji 文本的判定有出入；
建议用 `text.encode_utf16().count()` 保持一字不差，或直接 `chars().count()` 并记为已知偏差。

**粘贴主流程** `pasteToTerminalInner`（`terminalCache.ts:716-774`）：

```
target = resolvePasteTarget(ptyId)              // local / wsl / ssh，见 pastePath.ts:53-75
if clipboardHasImage():
    localPath = invoke('read_clipboard_image')  // Win32 读 DIB → temp PNG；失败 → null
    if localPath: path = await mapPastedFilePath(localPath, target); write(`"${path}"`); return
                  ↳ 失败 → notifyPasteFailure(target, e); return   // 不往终端写任何东西
    else: write('\x1bv'); return                // 回退 Alt+V 让 AI 工具自己处理
text = await readText(); if !text: return
if enabled && isLongText(text, line, char):
    try: localPath = invoke('save_clipboard_text', {text})
         path = await mapPastedFilePath(localPath, target)
         write(`"${path}"`); return
    catch e: notifyPasteFailure(target, e)      // ← 提示后**继续往下**，粘原文（老行为）
cached = getCachedTerminal(ptyId); if cached: cached.term.paste(text); return
await enqueuePtyWrite(ptyId, text)
```

**重入保护**（`terminalCache.ts:680-682, 706-714`）：`pasteInFlight: Set<ptyId>`，
远程上传要几百毫秒到几秒，期间连按 Ctrl+V 会让多条路径乱序插入命令行 → 直接丢弃重入那次。

**文件落哪**（`src-tauri/src/clipboard.rs`）：
- 目录：`std::env::temp_dir()/mini-term-clipboard/`（图片与文本**共用**）
- 文件名：`paste-{unix_millis}.txt`（`:321-327`）；图片是 `clip-*.png`
- 清理：`cleanup_old_clipboard_images()`（`:284-299`）启动时调一次，删 mtime > 24h 的**全部**文件
- 写失败文案（Rust 侧硬编码中文，不走 i18n）：`"创建临时目录失败: {e}"` / `"写入临时文件失败: {e}"`

**终端里写什么**：`"{path}"`（**带英文双引号**，兼容含空格路径），经 `enqueuePtyWrite`
写入 PTY；不追加空格、不追加回车。

**三类 pane 的路径映射** `mapPastedFilePath`（`src/utils/pastePath.ts:85-106`）：

| target | 处理 |
|---|---|
| `local` | 原样返回 Windows 路径 |
| `wsl` | `windowsPathToWsl(localPath) ?? localPath`（转不了就原样，退回改动前行为） |
| `ssh` | `invoke('ssh_remote_upload_paste', {connectionId, projectPath, localPath, destDir})`，`destDir = config.remotePasteDir?.trim() \|\| '.mini-term/pasted'` |

判定口径（`pastePath.ts:53-75`）刻意与后端 `create_pty` 的启动分支一致：
`project.sshConnectionId` 有值 → ssh；`isWslPath(project.path) \|\| paneRunsWslShell(shellName)` → wsl；
否则 local。`paneRunsWslShell`（`:40-50`）取 shell 命令的 **basename** 比对 `wsl`/`wsl.exe`
（避免 `C:\Windows\System32\wsl.exe` 漏判、`wslconfig.exe` 误判）。

**失败 toast** `notifyPasteFailure`（`terminalCache.ts:690-696`）：

```
pushNotification({ projectId: target.projectId, projectName: target.projectName,
                   kind: 'paste-error', message: t('terminal.pasteUploadFailed', {detail}) })
```

⚠️ **只有 `target.kind === 'ssh'` 时才有 `projectId/projectName`**（`PasteTarget` 的 local/wsl
变体没这两个字段，`pastePath.ts:28-37`）——本地写盘失败时 `notifyPasteFailure` 会拿到
`undefined`，是原版的一个隐性缺陷，迁移时应补一个兜底项目名。
i18n `terminal.pasteUploadFailed`（dict.rs `:1429` / `:1440`，插值 `{detail}`）**已在**。

**GPUI 现状**：
- 粘贴实现 `mt-ui/src/terminal/view.rs:428-436`：`cx.read_from_clipboard().text()` →
  `paste_to_bytes`（bracketed paste 包裹，`input.rs:219`）→ `write` —— **零阈值逻辑、零图片支持**
- 入口两处：`Ctrl+Shift+V`（`view.rs:468-470`）与右键菜单「粘贴」（`pane.rs:581-583`）
- 落点建议：在 `mt-app` 侧包一层 `paste_to_pane(pty_id, ...)`，读 config 判阈值 →
  写 temp 文件 → 写 `"{path}"`；`mt-ui` 的 `paste()` 保持纯粹（它不该知道 AppConfig）
- 图片粘贴（Win32 DIB 解析，`clipboard.rs` 的 `parse_dib` 那套含加固与单测）**属另一缺口**，
  本批只做文本长粘贴即可满足 audit #30 的措辞；如做图片，`clipboard.rs:1-280` 可整段移植
- SSH 上传分支依赖 mt-ssh 进 crates/（audit #28），本批**只做 local + wsl 两路**，
  ssh 路留 `todo!()` 或跳过（判据里 `sshConnectionId` 在 GPUI 的 ProjectConfig 里是否存在需回读）

## B.5 WSL 启动器重写提示

**后端**（`src-tauri/src/pty.rs`）：`decide_wsl_override(cwd)`（`:44`）识别三种 WSL UNC 形式
（`\\wsl$\`、`\\wsl.localhost\`、`\\?\UNC\wsl$\`，有 4 条单测 `:2316-2355`），命中即
无视用户配置的 shell 改用 `wsl.exe -d <distro> --cd <unix-path>`，并
`emit("wsl-shell-override", {ptyId, distro, unixPath})`（`:1054-1057`）。

**前端**（`App.tsx:367-379`）：

```ts
pushNotification({
  projectId: '__wsl_info__',        // 占位，不参与跳转
  projectName: `WSL: ${payload.distro}`,
  kind: 'wsl-info',                 // 已屏蔽点击跳转（ToastContainer.tsx:35-38）
  message: t('app.wslOverride', { path: payload.unixPath }),
});
```

5s 自动消失（走通用 toast TTL）。i18n `app.wslOverride`（dict.rs `:61` / `:105`，
zh "已检测到 WSL 项目,使用 wsl.exe 启动终端 ({path})"）**已在**。

**GPUI 现状**：
- 判定逻辑**已移植**：`mt-pty/src/launch.rs:39-43 decide_wsl_override` → `mt_core::parse_wsl_unc`，
  `launch::plan` 把结果放进 `LaunchPlan.wsl_override`（`launch.rs:97-116`）
- 结果**已存到会话上**：`PtySession::wsl_override()`（`mt-pty/src/lib.rs:343-345`）
- ❌ **mt-app 从不读它**（全仓零调用）→ 提示 toast 缺失
- 落点：`store.rs` 的建 PTY 处（`new_terminal` / `hydrate_project`）拿到 session 后判
  `wsl_override().is_some()` → 推一条 info toast。注意"一次性"语义：每个新 PTY 各推一次
  （原版同款，不去重）

## B.6 启动埋点

**两侧配合的统一时间轴**（Rust 进程启动为 T0）：

- Rust（`src-tauri/src/startup_trace.rs`，51 行）：
  - `init()` 在 `run()` 最前调一次，`T0: OnceLock<(Instant, f64)>` 双记单调钟 + epoch ms
  - `mark(label)` → `eprintln!("[startup +{:>5}ms] rust: {}", t0.elapsed().as_millis(), label)`
  - `startup_report(marks: Vec<(String, f64)>)` command：把前端的 epoch ms 换算成相对 T0，
    排序后统一打印，末尾打一行"时间线上报完毕"
- 前端（`src/utils/startupTrace.ts`，45 行）：
  - `markStartup(label)` 推 `[label, Date.now()]`
  - `flushStartupTrace()` 只跑一次（`flushed` 标志，防 StrictMode 双跑），
    补三个 Performance 锚点：`window.__earlyThemeTs`（index.html 内联首个脚本）、
    `performance.timeOrigin`（navigationStart）、`nav.responseEnd` / `nav.domInteractive`
  - 调用点（`App.tsx`）：`App() first render`（`:74`，模块级 `firstRenderMarked` 防双记）、
    `load_config invoke`（`:123`）、`load_config resolved`（`:136`）、
    `config applied (layout restored)`（`:198`）、`show() call (main UI first frame done)`（`:207`）
  - `show()` 在**双 rAF** 之后调（`:204`），确保 React 首帧布局完成；`show().then(flushStartupTrace)`

**GPUI 现状**：❌ 完全没有（`main.rs` 零命中 startup/Instant）。

**迁移建议**：GPUI 是单进程，前后端时间轴合一，整套 epoch 换算可删。
留一个 `mt-app/src/startup.rs`：`static T0: OnceLock<Instant>` + `mark(label)` eprintln，
在 `main()` 最前 / config 读完 / 首帧 render 结束三处打点即可。
**优先级最低**（纯诊断设施，不影响功能对等），可作为本批可选项。

## B.7 dirKinds 技术栈探测

### 探测规则全表（`src/utils/projectKind.ts:39-58`，从具体到泛化，命中即返回）

| # | 判据（项目根目录**一层**的文件名集合） | 结果 |
|---|---|---|
| 1 | `pom.xml` \|\| `build.gradle` \|\| `build.gradle.kts` | `java` |
| 2 | `Cargo.toml` | `rust` |
| 3 | `go.mod` | `go` |
| 4 | `pyproject.toml` \|\| `requirements.txt` | `python` |
| 5 | `pubspec.yaml` | `flutter` |
| 6 | `composer.json` | `php` |
| 7 | `package.json` + deps 细分 | 见下 |
| 8 | 都不命中 | `null`（已探测但识别不出） |

`package.json` 的 deps 细分（**顺序即优先级**，`:50-55`）：
`vue` → `vuejs`；`next` → `nextjs`；`react` → `react`；`svelte` → `svelte`；`vite` → `vite`；
都没有 → `nodejs`。deps = `{...pkg.dependencies, ...pkg.devDependencies}` 合并表
（`parsePackageDeps`，`:61-72`；JSON 解析失败返回 `undefined`，退化成 `nodejs`）。

12 种 kind 的展示名（`PROJECT_KIND_LABELS`，`:18-31`，专有名词**不进 i18n**）：
Java / Rust / Go / Python / Flutter / PHP / Vue / Next.js / React / Svelte / Vite / Node.js。
手动指定菜单的顺序 `PROJECT_KINDS`（`:13-15`，常用在前）：
`java, rust, go, python, nodejs, react, vuejs, nextjs, svelte, vite, flutter, php`。

### 缓存与失效（`src/hooks/useProjectKinds.ts`）

| 项 | 实现 |
|---|---|
| 缓存本体 | `store.dirKinds: Map<path, ProjectKind \| null>` + `dirKindsVersion: number`（`store.ts:708-709, 755-765`） |
| 三态语义 | `undefined` = 尚未探测；`null` = 探测过但识别不出；`ProjectKind` = 命中 |
| 在途去重 | 模块级 `pending: Set<path>`（`useProjectKinds.ts:20`）——不是可订阅状态，故不进 store |
| 探测动作 | `detectLocal(path)`（`:26-46`）：`list_directory` 取一层 → 若有 `package.json` 再 `read_file_content` 读它（`isBinary`/`tooLarge` 时跳过）→ `classifyProject` |
| 批量入口 | `ensureDirKinds(paths)`（`:55-71`），失败也写 `null` 进缓存（不重试） |
| **远程项目不探测** | `projects.filter(p => !p.sshConnectionId)`（`:85`），远程行固定显示 SSH 图标 |
| 失效触发 | `fs-change` 事件：路径的**basename** ∈ `PROJECT_MARKER_FILES`（10 个标记文件，`projectKind.ts:34-37`）且其**父目录**正好等于某个本地项目根 → `removeDirKind(project.path)`（`:88-103`） |
| 补探 | `removeDirKind` 令 `dirKindsVersion` +1 → `useEffect([projects, version])` 重跑 `ensureDirKinds`（`:82-86`） |
| 路径归一 | `normPath`：`\`/`/` 统一成 `/`、去尾斜杠（`:22-24`） |
| 共用缓存 | 项目根与文件树里的子工程目录共用同一份（`:53-54` 注释） |

⚠️ 只有**活跃项目的根目录**被 watch（`fs.rs` 的 `watch_directory`），
注释（`:13-14`）明说"这正是唯一能在应用内改动这些文件的场景"——不必追求全局监听。

### GPUI 现状

- ✅ `mt_ui::icons::TechIcon` + `ProjectKind`（12 种）已实现（K 批），`ALL_PROJECT_KINDS` 可枚举
- ✅ 手动指定：项目行右键 →「项目类型」子菜单 → `store.set_project_kind_override()`
  （`store.rs:321`，写 `kind_override: Option<String>`），`project_list.rs:276` 消费
- ❌ **自动探测整条链缺失**：`project_list.rs:10` 原注释已记
  "技术栈**只认 `kindOverride`**：原版还有一路 `useProjectKinds` 探测"，
  `:105` 记"GPUI 侧还没有探测那一路，所以不带括号"
- 现成材料：`mt_project::fs::list_directory(project_root, path)`（`fs.rs:276`）与
  `read_file_content`（`fs.rs:322`）签名与 Tauri 侧一致；`mt-project/src/watch.rs` 有文件监听
- 建议：`classify_project(files: &HashSet<String>, deps: Option<&Map>) -> Option<ProjectKind>`
  放进 `mt-project`（纯函数，好写单测，逐条照抄上表），缓存放 `AppStore`
  （`HashMap<PathBuf, Option<ProjectKind>>`，GPUI 有 `cx.notify()` 天然替代 `dirKindsVersion`），
  探测走 `cx.background_executor().spawn` 回主线程写入（P 批文件操作同款范式）

## B.8 pane 进场动画（含 reduced-motion 豁免口径）

**动画本体**（`styles.css`）：

```css
@keyframes paneEnter {           /* :302-307 */
  from { opacity: 0; transform: scale(0.97); }
  to   { opacity: 1; transform: scale(1); }
}
.pane-enter {                    /* :352-354 */
  animation: paneEnter var(--motion-pane-enter) var(--ease-overlay-in);
}
```

- `--motion-pane-enter: 0.26s`（`:73`）
- `--ease-overlay-in: cubic-bezier(0.16, 1, 0.3, 1)`（easeOutExpo，`:77`）
- 缩放幅度压到 0.97 的理由（`:302-303` 原注释）："再大就会让 xterm 的 canvas 在这几帧里明显发虚"
- **不加 `forwards`** —— 动画一结束 transform 就撤掉，避免长期制造 containing block

**挂载点**（`PaneGroup.tsx:391-393`）：分屏格子的最外层 `div`，
注释："项目切到后台是 display:none 留着不卸载，不会重播；只有真正新建/重排分屏时这层才重挂载"。

**姊妹动画**（同批一起做更划算）：`.terminal-swap-in`（`styles.css:292-295, 348-350`，
`--motion-terminal-swap: 0.2s`，`translateY(6px)` 淡入），挂在
`TerminalInstance.tsx:418` 的 `<div key={ptyId}>` 上 —— `key` 变化触发重建，切 pane 时重播；
**外层带 `data-pty-id` 的盒子刻意不参与动画**（查找条与拖拽命中按它定位，跟着位移会飘）。

### reduced-motion 豁免口径（已确认）

`styles.css:391-402` 有一条**通配兜底**：`*, *::before, *::after { animation-duration: 0.01ms !important;
animation-iteration-count: 1 !important; transition-duration: 0.01ms !important }`。

`.pane-enter` **在豁免名单里**（`styles.css:441-443`）：

```css
@media (prefers-reduced-motion: reduce) {
  .pane-enter { animation-duration: var(--motion-pane-enter) !important; }
}
```

同名单还有 `.overlay-backdrop` / `.overlay-panel` / `.overlay-drawer` / `.prompt-*` /
`.overlay-menu` / `.ctx-menu` / `.terminal-swap-in` / `.panel-swap-in` / `.drawer-tab-indicator` /
`.git-section-*`。豁免理由（`:415-421` 原注释 + 记忆库 `project_reduced_motion_env.md`
2026-07-29 用户拍板）：
> Windows 上「关闭窗口动画」（`SPI_GETCLIENTAREAANIMATION=FALSE`）就会让 WebView2 落进 reduce 分支 ——
> 走到这里的多半不是前庭敏感用户，而是把系统视觉效果调成「最佳性能」的人。

**仍然停掉的**：`.animate-blink`（状态灯/更新红点的无限闪烁）；
**仍然继续动但放慢到 2.4s 的**：`.animate-status-spin` / `.animate-spin`（`:409-413`，
"一个停住的 spinner 不是安静，是在说谎"）。

**→ GPUI 侧口径（结论）**：GPUI **不读** `prefers-reduced-motion`（gpui 0.2.2 无此 API，
`SPI_GETCLIENTAREAANIMATION` 也没人查）。既然 pane 进场/终端切换/浮层进出场在原版都已豁免，
**GPUI 侧一律照常播，不需要任何系统开关探测**——这与用户机器上的实际观感一致。
唯一需要留意的是 `animate-blink` 那类无限闪烁（更新红点、标题栏 working 灯）：
原版在该用户机器上是**看不见闪的**，GPUI 若照实做闪烁会是"新出现的动效"。
建议：working 灯与更新红点的闪烁**照做**（它们是本批新增的可见状态指示，
且 audit 的「其他细项」里已把"徽标闪烁动画未做"列为缺口），如用户反馈刺眼再关。

**GPUI 现状**：❌ 无 pane 进场动画。`with_animation` 在仓内有两处范式：
`mt-ui/src/icons/status.rs:278`（旋转，`Animation::new(SPIN_PERIOD).repeat()`）与
`mt-ui/src/terminal/element.rs:490`（滚动条淡出**刻意不用** `with_animation`，
改"按时间算 alpha + 没淡完再要一帧"，理由是防持续请求帧）。
pane 进场是一次性动画，用 `with_animation` 即可，但 ⚠️ **id 必须带 pane_id 保跨帧稳定**
（`status.rs:52` 的告警），否则分屏重排时动画状态串台。

## B.9 GPUI 现状差异清单（#30）

| # | 子项 | GPUI 现状 | 缺口 | 依赖 |
|---|---|---|---|---|
| 1 | 关窗确认 | ❌ 无 `on_window_should_close`；只有 `on_app_quit` 存盘 | 跨项目盘点 + 异步确认 + `force_close` 标志 | 复用 `is_ai_alive` / `Confirm` |
| 2 | 启动版本检查 | ❌ 无 HTTP 客户端 | `reqwest`(rustls) + 边条按钮 + 打开浏览器 | 需动 Cargo.toml |
| 3 | 更新提醒红点 | ❌ | `VectorIcon` 两 path + accent 点 + 闪烁 | — |
| 4 | FirstRunGuide | 🟡 只有一行 `app.emptyState` | 标题/副标题/两按钮/三键位提示 | 「添加 SSH 远程」依赖 #28 |
| 5 | 长粘贴转文件 | ❌ 零消费（4 config 字段已在） | 阈值判定 + temp 落盘 + `"{path}"` 写入 + 24h 清理 | ssh 路依赖 #28 |
| 6 | 剪贴板图片粘贴 | ❌ | 本批**不做**（audit #30 未列） | — |
| 7 | WSL 重写提示 | 🟡 `PtySession::wsl_override()` 已有，无人读 | 一条 info toast | 依赖 §C 的 toast kind |
| 8 | 启动埋点 | ❌ | 单进程版可大幅简化 | 优先级最低 |
| 9 | dirKinds 探测 | 🟡 只认 `kind_override` | `classify_project` + 缓存 + fs-change 失效 | `mt_project::fs` 已就绪 |
| 10 | pane 进场动画 | ❌ | `with_animation`，id 带 pane_id | — |
| 11 | 终端切换动画 | ❌ | 同上（姊妹项，顺手做） | — |
| 12 | i18n | ✅ 本节所有 key 全在 dict.rs | 无 | — |

---

# C. Toast 与提示音（其他细项）

## C.1 原版 Toast 完整规格

### C.1.1 数据模型

`AiCompletionNotification`（`src/types.ts:306-319`）：

```ts
{ id: string; projectId: string; projectName: string; timestamp: number;
  kind?: 'ai-completion' | 'ai-attention' | 'wsl-info' | 'mobile-session' | 'paste-error';
  message?: string }
```

`kind` 缺省 = `ai-completion`（老记录同样算在内，`store.ts:1035` 的判据显式写了 `n.kind === undefined`）。

### C.1.2 五种 kind 全表

| kind | 图标字符 | 图标类 | 卡片类 | 正文 | 点击行为 | 推送方 |
|---|---|---|---|---|---|---|
| `ai-completion`（缺省） | `✓` | `.toast-icon`（success 绿） | `.toast-card` | `t('toast.aiDone')` | 切项目 + 关闭 | `store.ts:1040-1045` |
| `ai-attention` | `!` | `.toast-icon--attention`（warning 黄） | `.toast-card--attention`（左边框黄） | `t('toast.aiAttention')` | 切项目 + 关闭 | `store.ts:1086-1092` |
| `wsl-info` | `i` | `.toast-icon--info` | `.toast-card` | `n.message` | **仅关闭** | `App.tsx:372-377` |
| `mobile-session` | `i` | `.toast-icon--info` | `.toast-card` | `n.message` | 切项目 + 关闭 | `mobileStartSession.ts` |
| `paste-error` | `!` | `.toast-icon--error` | `.toast-card` | `n.message` | **仅关闭** | `terminalCache.ts:690-695` |

判定代码（`ToastContainer.tsx:19-26, 42-63`）：
`isInfo = isWslInfo || isMobileSession`；图标优先级 `paste-error > ai-attention > isInfo > 默认`；
正文优先级 `isInfo || isPasteError → message` `> isAttention → aiAttention` `> aiDone`。

### C.1.3 容器与生命周期

| 项 | 值 | 出处 |
|---|---|---|
| 最多渲染 | `notifications.slice(0, 5)`，超出**排队**（不丢，等前面消失后补位） | `ToastContainer.tsx:12` |
| 位置 | `position:fixed; right:16px; bottom:16px; flex-col; gap:8px; z-index:70; pointer-events:none`（卡片自身 `pointer-events:auto`） | `styles.css:538-547, 562` |
| 无内容 | 整个容器不渲染（`return null`） | `:14` |
| 无障碍 | `role="status" aria-live="polite"` | `:17` |
| TTL | `NOTIFICATION_TTL_MS = 5000` | `store.ts:592` |
| 定时器管理 | 模块级 `notificationTimers: Map<id, timeout>`，**在 store 内部管**（"避免组件 useEffect 重置问题"） | `store.ts:593-609, 1235-1236` |
| 悬停暂停 | `onMouseEnter → pauseNotification(id,true)`（清定时器）；`onMouseLeave → false`（**重新计满 5s**，不是续剩余） | `ToastContainer.tsx:32-33`；`store.ts:1247-1250` |
| × 关闭 | `.toast-close` 按钮，`e.stopPropagation()` 后 `dismissNotification`，`aria-label`/`title` = `t('toast.dismiss')` | `:65-74` |
| 卡片点击 | 非 `wsl-info`/`paste-error` → `setActiveProject(n.projectId)`；一律 `dismissNotification` | `:34-40` |
| 关项目清理 | 关闭项目时 `notifications.filter(n => n.projectId !== id)` | `store.ts:859` |

### C.1.4 样式关键值（`styles.css:537-615`）

```
.toast-card  width:280px; bg:var(--bg-elevated); border:1px solid var(--border-default);
             border-left:3px solid var(--color-success); border-radius:6px; padding:10px 12px;
             display:flex; align-items:center; gap:10px; box-shadow:var(--shadow-overlay);
             font-family:system-ui,sans-serif; cursor:pointer; pointer-events:auto;
             animation:toastSlideIn 0.25s ease-out; transition:transform 0.15s;
.toast-card:hover { transform: translateX(-2px); }
.toast-card--attention { border-left-color: var(--color-warning); }
.toast-icon  20×20; border-radius:50%; bg:var(--color-success); color:var(--bg-base);
             font-weight:700; font-size:0.85rem; flex-shrink:0;
.toast-icon--info { background: var(--color-info); }
.toast-icon--error { background: var(--color-error); }
.toast-icon--attention { background: var(--color-warning); }
:root[data-theme="light"] .toast-icon { color: #ffffff; }
.toast-name  color:var(--text-primary); font-size:0.92rem; font-weight:600; 单行省略
.toast-desc  color:var(--text-secondary); font-size:0.77rem; margin-top:1px;
.toast-close color:var(--text-muted); font-size:1.08rem; padding:0 4px; cursor:pointer;
             hover → var(--text-primary)
@keyframes toastSlideIn { from { opacity:0; transform:translateX(100%) } to { opacity:1; translateX(0) } }
```

**布局是两行**：`.toast-name` = 项目名，`.toast-desc` = 描述；左侧圆形图标，右侧 ×。
**无退场动画**（只有进场 `toastSlideIn`）。

### C.1.5 推送去重（属通知状态机，GPUI 已实现，仅列对照）

- 完成 toast：同项目已有 `kind === undefined || 'ai-completion'` 的未消失 toast → 不推
  （`store.ts:1033-1036`；注释明说不能只按 projectId 判，否则待确认 toast 会吞掉完成 toast）
- 待确认 toast：同项目已有 `kind === 'ai-attention'` 的 → 不推（`store.ts:1080-1082`）
- 两者都**只在非激活项目**推（`:1022, 1078`）

## C.2 GPUI 现有 toast 实现对照

**实现位置**：`crates/mt-app/src/main.rs:290-341`（`Workspace::deliver_alert`），
走 `gpui_component::notification::Notification` + `window.push_notification(note, cx)`，
渲染层是 `Root::render_notification_layer`（`main.rs:22` 的架构图）。

现状代码要点：

```rust
ToastKind::Completion => Notification::success(format!("{} · {}", project_name, t("toast","aiDone")))
    .id1::<CompletionToast>(key)          // key = project_id，完成/待确认各一个键空间
    .on_click(on_click)                   // 跳到该项目的待办 pane
ToastKind::Attention  => Notification::warning(...).id1::<AttentionToast>(key).on_click(on_click)
```

**gpui-component 0.5.1 的 Notification 能力**（读 `notification.rs` 全文）：

| 能力 | 状态 | 出处 |
|---|---|---|
| 自动消失 5s | ✅ 恰好 5s，`Timer::after(Duration::from_secs(5))` | `:404-411` |
| **悬停暂停** | ❌ **没有**。`NotificationList` 的 `on_hover` 只置 `expanded` 字段，且该字段**在 render 里根本没被用**（死字段） | `:456-458` |
| × 关闭按钮 | 🟡 有，但 `invisible()` + `group_hover` 才显形，图标 `IconName::Close` → **本仓渲染空白** | `:322-343` |
| 最多显示 | 🟡 **10** 条（`.iter().rev().take(10).rev()`），原版是 5 | `:451` |
| 位置 | ❌ **右上**（`absolute().top_4().right_4()`），原版右下 | `:453` |
| 宽度 | `w_112()` = 448px，原版 280px | `:284` |
| 图标 | ❌ `NotificationType` 的四个图标全是 `IconName`（Info/CircleCheck/TriangleAlert/CircleX）→ **空白** | `:33-38` |
| 两行布局 | ✅ `.title()` + `.message()` 支持，现有代码没用 title，拼成一行 | `:188, 300-311` |
| 点击 | ✅ `on_click` 先 dismiss 再回调 | `:317-322` |
| 进出场动画 | ✅ 0.25s，进场 `translateY(-45px)` 淡入，出场右移 45px 淡出（原版只有进场，且是 X 方向 100%） | `:341-357` |
| 去重 | ✅ `id1::<T>(key)` 同 id 覆盖（原版是"已有就不推"，**语义相反**：gpui-component 会**替换**，原版会**忽略**） | `:390-392` |
| 自定义样式 | ✅ `Styled` + `refine_style(&self.style)`，可覆盖宽度/圆角/边框/背景 | `:273, 314` |
| 关闭 autohide | ✅ `.autohide(false)` | `:208-211` |

## C.3 Toast 缺口逐条

| # | 缺口 | 现状 | 修法 |
|---|---|---|---|
| 1 | **悬停暂停** | ❌ 组件库无此能力（`expanded` 是死字段） | 组件库改不了 → **自建 toast 层**（见 C.4）；或 `.autohide(false)` + 自己起 `Timer`，`on_hover` 时取消/重起（需要拿到 `Entity<Notification>`，`push_notification` 不返回句柄 → 走不通） |
| 2 | **× 关闭可见性** | 只在 hover 时显形，且图标空白 | 自建：常驻 `×` 字符（原版就是文本 `×`，不是 svg） |
| 3 | **最多 5 条** | 组件库写死 10 | 自建；或每次 push 前自己数（拿不到列表 → 走不通） |
| 4 | `wsl-info` kind | ❌ `ToastKind` 只有 `Completion`/`Attention`（`notify.rs:78-84`） | 加枚举 + info 图标 + **点击不跳项目** |
| 5 | `mobile-session` kind | ❌ | 加枚举（依赖移动端批 #22） |
| 6 | `paste-error` kind | ❌ | 加枚举 + error 图标 + 点击不跳项目 |
| 7 | 点击跳项目 | ✅ 已做（`main.rs:308-317`，`next_attention_target(Some(pid))` → `set_active_project` + `activate_pane`） | 新 kind 要**屏蔽**这条 |
| 8 | 位置/尺寸/配色 | 右上 448px + gpui-component theme | 自建：右下 16/16，280px，`ui::` palette |
| 9 | 图标 | 空白 | 自建：圆形 20px 底 + `✓`/`!`/`i` 文本字符（原版就是文本！`ToastContainer.tsx:53`） |
| 10 | 去重语义 | 替换 vs 忽略 | 自建时按原版"已有就忽略"（`store.ts:1033`） |
| 11 | 两行文案 | 现在拼成一行 `"{项目名} · {文案}"` | 自建：项目名 0.92rem/600 + 描述 0.77rem |
| 12 | 关项目清理 | ❌ | 自建时在 `remove_project` 里过滤 |

### C.4 建议：自建 toast 层

理由：12 条缺口里有 4 条（悬停暂停 / 上限 5 / × 常驻 / 去重语义）**组件库结构上做不到**，
且图标空白问题与 M/P 批同源（无 AssetSource）。自建代价可控 —— 原版 toast 只有
80 行 TSX + 78 行 CSS，且**没有任何图标资源**（全是文本字符）。

落点建议：`crates/mt-app/src/toast.rs`，一个 `Entity<ToastLayer>`：
- 状态：`VecDeque<ToastItem>{ id, project_id, project_name, kind, message, }` + 每条一个
  `Task<()>` 定时器句柄（`cx.spawn` + `Timer::after(5s)`；悬停时 drop 句柄暂停、离开时重建）
- 渲染：`div().absolute().right(px(16.)).bottom(px(16.))` 挂在 `Workspace::render` 的
  根 `relative` 容器里（背景图层之后、Root 的 dialog/notification 层之前），
  取前 5 条，`.occlude()` 防穿透
- 进场动画：`with_animation` 0.25s `translateX(100%) → 0` + 淡入，
  ⚠️ id 用 toast id 保稳定
- 保留 `Root::render_notification_layer` 不动（gpui-component 内部其它地方可能用），
  只是 mt-app 不再 `push_notification`

**i18n**：`toast.aiDone` / `toast.aiAttention` / `toast.dismiss` 三条**已在**
dict.rs `:1503-1511`。

## C.5 提示音

### C.5.1 原版双音的确切参数（`src/utils/notificationSound.ts:10-34`）

Web Audio，两段**顺序播放**的正弦波，共享一个懒创建的 `AudioContext`（`:3-8`，
`state === 'suspended'` 时 `resume()`）：

| 段 | 频率 | 波形 | 起始 | 结束 | 增益包络 |
|---|---|---|---|---|---|
| 1 | **880 Hz**（A5） | `sine` | `now` | `now + 0.12` | `setValueAtTime(0.3, now)` → `exponentialRampToValueAtTime(0.01, now+0.12)` |
| 2 | **660 Hz**（E5） | `sine` | `now + 0.13` | `now + 0.28` | `setValueAtTime(0.3, now+0.13)` → `exponentialRampToValueAtTime(0.01, now+0.28)` |

- 总时长 **280ms**，两段之间有 **10ms 静默**（0.12 → 0.13）
- 峰值增益 **0.3**（线性），指数衰减到 0.01（≈ -30dB），**无 attack**（瞬起）
- 音程：880→660 是**下行纯四度**（A5→E5）
- 自定义音（`:36-42`）：`convertFileSrc(path)` → `new Audio(url)`，`volume = 0.5`，
  播完 `audio.src = ''` 释放；`play()` 失败静默
- 入口 `playNotificationSound(soundPath?)`（`:44-54`）：有路径走自定义，否则默认双音，
  整体 try/catch 静默

**调用点**：`store.ts:1010-1014`（完成）与 `:1062-1066`（待确认），
都在 `queueMicrotask` 里、受 `config.aiCompletionSound` 开关管辖，**不区分窗口焦点**。

### C.5.2 GPUI `notify.rs` 现状与 Win32 能力边界

`crates/mt-app/src/notify.rs:234-267`：

```rust
pub fn play_sound(custom_path: Option<&str>) {
    if let Some(path) = custom_path.filter(|p| p.to_ascii_lowercase().ends_with(".wav")) {
        let ok = unsafe { PlaySoundW(PCWSTR(HSTRING::from(path).as_ptr()), None,
                                     SND_FILENAME | SND_ASYNC | SND_NODEFAULT) };
        if ok.as_bool() { return; }
    }
    unsafe { let _ = MessageBeep(MB_OK); }        // ← 缺口：不是 880→660 双音
}
```

已知偏差（模块注释 `:236-237` 已记）：自定义音**只认 `.wav`**（原版支持 mp3/ogg），
无自定义音时回落 `MessageBeep(MB_OK)`（系统"星号"音，跟随用户的声音方案，可能是静音）。

**Win32 能力边界**：

| API | 能否放任意波形 | 备注 |
|---|---|---|
| `MessageBeep(MB_OK)` | ❌ 固定系统音 | 用户改声音方案就变；可能被设成"无" |
| `Beep(freq, ms)`（kernel32） | 🟡 **能指定频率与时长**（37–32767 Hz） | **同步阻塞**调用线程；Win7+ 走默认音频设备（不再是主板蜂鸣器）；**方波，无音量控制、无包络**；两段之间要 `Sleep(10)` |
| `PlaySoundW(SND_MEMORY)` | ✅ **能放内存里的 WAV** | 传 `SND_MEMORY \| SND_ASYNC`，指针指向完整 RIFF/WAVE 字节，**播放期间内存必须存活**（SND_ASYNC 下要 leak 或保住 buffer） |
| `PlaySoundW(SND_FILENAME)` | ✅ 磁盘 WAV | 当前实现；写临时文件有 I/O 与清理负担 |
| XAudio2 / WASAPI | ✅ 完全可控 | 需要 `windows` crate 加大量 feature + COM 初始化，为一个提示音过重 |

### C.5.3 推荐方案：**内存合成 WAV + `PlaySoundW(SND_MEMORY | SND_ASYNC)`**

理由：
- 能 **1:1 复刻**双音的频率/时长/间隔/指数衰减包络（`Beep` 做不到包络与音量，且阻塞）
- 不落盘 → 无临时文件、无清理、无杀软误报
- 不加任何新依赖：`Win32_Media_Audio` feature **已经开着**（`mt-app/Cargo.toml`），
  `PlaySoundW` 已在用，只多一个 `SND_MEMORY` 常量
- 合成代码约 40 行纯 Rust，可单测（校验 RIFF 头、采样数、包络端点）

实现要点：

```
采样率 44100，单声道，16-bit PCM（最保守的兼容组合）
总长 0.28s → 12348 采样
s(t) = 段内 sin(2π·f·t) · env(t)
env(t) = 0.3 · (0.01/0.3)^(t/dur)      // 指数衰减，与 exponentialRampToValueAtTime 同形
段1: f=880, t∈[0, 0.12)
静默: t∈[0.12, 0.13)
段2: f=660, t∈[0.13, 0.28)   （段内 t 从 0 重新起算）
i16 = (s · 32767).clamp(-32768, 32767)
```

WAV 头 44 字节：`"RIFF"` + (36+data_len) + `"WAVE"` + `"fmt "` + 16 + fmt=1(PCM) +
ch=1 + rate=44100 + byte_rate=88200 + block_align=2 + bits=16 + `"data"` + data_len。

**内存存活**：`SND_ASYNC` 下 `PlaySoundW` 立即返回、后台继续读那块内存。
用 `static WAVE: OnceLock<Vec<u8>>`（合成一次、永久存活）即可 —— 波形是常量，
天然适合 `OnceLock`，既解决存活问题又省掉每次合成的开销。
⚠️ 若将来支持"自定义双音参数"，改成 `Box::leak` 或每次播放前 `PlaySoundW(None,...)` 停旧的。

**回落链**（保持现有语义 + 补双音）：

```
1. custom_path 且 .wav 存在 → PlaySoundW(SND_FILENAME | SND_ASYNC | SND_NODEFAULT)，成功即返回
2. → PlaySoundW(SND_MEMORY | SND_ASYNC | SND_NODEFAULT, 内置双音)      ← 本次新增
3. → MessageBeep(MB_OK)                                                ← 兜底不变
```

**被否掉的方案**：
- `Beep(880,120); Sleep(10); Beep(660,150)` —— **同步阻塞主线程 280ms**（GPUI 单线程 UI，
  会掉 17 帧）；即使 `spawn` 到后台线程，方波音色与原版正弦差异明显、且无音量控制
- 落临时 `.wav` 文件 —— 多一份 I/O + 清理（clipboard 那套 24h 清理已经证明这条路要配套设施）
- 引 `rodio`/`cpal` —— 为一个提示音拉一整棵音频依赖树，与"不引 tray-icon"同理由否掉

**非 Windows**：维持 `#[cfg(not(windows))] pub fn play_sound(_) {}` 空实现。

## C.6 GPUI 现状差异清单（Toast + 提示音）

| # | 项 | 现状 | 缺口 |
|---|---|---|---|
| 1 | toast 载体 | 🟡 `gpui_component::Notification` | 4 条结构性缺口 → 建议自建 `toast.rs` |
| 2 | 悬停暂停 | ❌ 组件库无 | 自建后自管 Timer |
| 3 | 上限 5 条 | ❌ 库内写死 10 | 自建 |
| 4 | × 关闭 | 🟡 hover 才显 + 图标空白 | 自建，用文本 `×` |
| 5 | 位置/尺寸 | ❌ 右上 448px | 右下 16/16、280px |
| 6 | 图标 | ❌ IconName 空白 | 20px 圆底 + `✓`/`!`/`i` 文本 |
| 7 | kind 全表 | 🟡 2/5（Completion、Attention） | 补 `wsl-info` / `mobile-session` / `paste-error` |
| 8 | 点击跳项目 | ✅ | 新增 3 kind 中 2 个要**屏蔽**跳转 |
| 9 | 进出场动画 | 🟡 库自带（Y 方向） | 自建后照抄 `toastSlideIn`（X 方向 100%） |
| 10 | 去重语义 | ❌ 替换 vs 忽略 | 自建按"已有就忽略" |
| 11 | 关项目清理 toast | ❌ | `remove_project` 里过滤 |
| 12 | 提示音双音 | ❌ 回落 `MessageBeep` | 内存合成 WAV + `SND_MEMORY` |
| 13 | 自定义音格式 | 🟡 只认 `.wav` | 已知偏差，本批**不改**（改需引音频解码库） |
| 14 | i18n | ✅ `toast.aiDone/aiAttention/dismiss` + `terminal.pasteUploadFailed` + `app.wslOverride` 全在 | 无 |

---

## 附：本批与其它批的接口

- **与 tray.md（#21）共用**：`collectAiProjects` 等价物（标题栏胶囊 + 托盘菜单**同一份**，
  只是 done 判据入参不同：胶囊传 `aiDoneOrder`、托盘传 `unreadDonePaneIds`）。
  谁先做谁建，建在 `store.rs` 上，签名建议
  `fn collect_ai_projects(&self, done: &dyn Fn(&str) -> bool) -> AiProjectSummary`。
- **与 settings-pages.md**：`longPaste*` 三字段的设置 UI 已在那边（§Section 2），
  本批只做**消费端**；「关于」页的手动检查更新同理。
- **依赖 #28（SSH 批）**：FirstRunGuide 的「添加 SSH 远程项目」按钮、
  长粘贴的 ssh 上传分支。两处都可先做本地路径、留位。
- **依赖 #22（移动端批）**：`mobile-session` toast kind。
- **可整个删掉的原版资产**：`src-tauri/src/window_snap.rs`（283 行）——
  gpui 的 `WindowControlArea` 内建等价能力。

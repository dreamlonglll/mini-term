# 项目 pane 预览缩略图 — 设计文档

日期：2026-08-08 · 分支：`feat/project-pane-preview` · 方案：A（hover 读 buffer 画迷你 canvas）

## 需求

鼠标悬停项目列表中的项目行，弹出悬浮卡显示该项目**所有 pane**（含分屏与隐藏 tab）的终端画面缩略图，
不切换项目即可瞥见各终端现场（AI 跑到哪、是否在等确认）。浮层打开期间预览持续刷新。

## 可行性依据（代码事实）

1. **buffer 永远实时**：xterm 实例按 ptyId 缓存于 `terminalCache.ts` 模块级 Map，React 卸载不销毁；
   全局 `pty-output` 监听把输出写进所有缓存实例的 buffer，与可见性无关。
2. **像素不可靠**：隐藏终端渲染暂停（RenderService._isPaused）且 WebGL 无 preserveDrawingBuffer，
   截 canvas 对隐藏 pane 拿到过期/空白像素 → 放弃截图路线，读 buffer 自绘。
3. **懒创建的 PTY**：本次启动未切到过的 pane 无 ptyId/无 buffer，只有持久化布局元数据
   （shellName/customTitle/aiSession/status）→ 渲染占位卡。

## 模块划分

| 模块 | 职责 |
|------|------|
| `src/utils/panePreview.ts` | 纯逻辑：从 buffer 结构性接口提取 viewport cell 网格（字符+前景色 RGB）；xterm 256 色索引 → RGB 解析。仿 `terminalSnapshot.ts` 的接口注入设计，可被 node --test 直测 |
| `src/utils/panePreviewCanvas.ts` | DOM 侧：把 cell 网格按等宽网格 fillText 到 canvas（内部分辨率按 8px 字号建，CSS 等比缩到卡宽） |
| `src/components/ProjectPanePreview.tsx` | 悬浮卡：portal 到 body、项目行右侧定位、按分屏叶子分组平铺 pane 卡（缩略图 + StatusDot + BrandIcon + 标题）、打开期间 500ms 重画、无终端 pane 显示占位 |
| `src/components/ProjectList.tsx` | 行级 hover 意图：onMouseEnter 起 250ms 定时器弹卡，leave/mousedown/拖拽（`isProjectDragging()`）即关 |

## 数据流

```
hover 项目行 (250ms) → projectStates.get(id).layout → collectLeaves()
  每 pane：ptyId → getCachedTerminal() → term.buffer.active(viewportY 起 rows 行)
    → extractPreviewGrid()（字符+前景色，色板取 term.options.theme）
    → drawPreviewGrid() 画 canvas → 卡片展示；无缓存终端 → 占位卡
  浮层打开期间 500ms interval 重画（预览是活的）
```

## 边界与取舍

- Claude/Codex TUI 在 alternate buffer：`buffer.active` 即当前画面，天然正确。
- 只画字符 + 前景色，不画背景色/粗体（首版取舍；升级保真度只需在绘制函数补 `getBgColor`）。
- 宽字符（中文/emoji）按 cell 列定位（`x = col × cellW`），占两列的字符画一次、跳过其 0 宽尾 cell。
- 悬浮卡纯展示不可交互，鼠标移出项目行即关；拖拽排序期间不弹。
- 从未打开的项目整卡占位（元数据仍有信息量）；远程断线 pane 显示断线前画面。
- 性能：不 hover 零开销；单项目数个 pane、每 pane 数千 cell 提取为毫秒级。

## 变更记录

- **2026-08-09 排版改版（方案 2）**：单列平铺在多 pane 时总高超屏（4 张 ≈ 1100px）。改为
  「微缩布局拼图」：浮层固定 520×(卡头+340)，按 SplitNode 树用 flex-grow 复现 sizes 比例
  嵌套排布，leaf 显示 active tab 画面（cover + 左下锚定裁右裁顶），隐藏 tab 以「+N」徽章
  示数并附其中最高优先级状态点。永不超屏，与切过去看到的终端区所见即所得；
  「所有 pane 均出画」退让为「active pane 出画 + 隐藏 tab 状态不漏报」。

## 验证

- `tests/panePreview.test.cjs`：真 xterm Terminal（node 下可跑，`tuiScrollback.test.cjs` 先例）写入
  ANSI 后断言提取网格的字符/颜色/宽字符/viewport 跟随行为；`npx tsc -p tsconfig.test.json` 后
  `node --test` 全量绿。
- `npm run build`（tsc + vite）通过；`npm run tauri dev` 人工验收悬停各类项目（活跃/后台/未启动/拖拽互斥）。

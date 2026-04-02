# 日间模式（浅色主题）设计文档

## 概述

为 mini-term 新增 Pure White 浅色主题，支持跟随系统自动切换，用户也可手动指定深色/浅色。终端区域是否跟随主题由用户独立控制。

## 需求汇总

| 决策项 | 选择 |
|--------|------|
| 切换方式 | 跟随系统 + 手动覆盖 |
| 浅色风格 | Pure White — 纯白/灰背景，铜色强调色保留 |
| 终端行为 | 用户可选：跟随主题 or 始终深色 |
| UI 入口 | 设置面板（TerminalConfigModal） |
| 过渡动画 | CSS transition 200-300ms 平滑过渡 |
| 语义颜色 | 浅色下适度加深以保证对比度 |

## 实现方案：CSS 变量切换

通过 `document.documentElement.dataset.theme` 切换 `"light"` / `"dark"`，CSS 变量在 `:root[data-theme="light"]` 中覆盖。

### 架构

```
AppConfig.theme: "auto" | "light" | "dark"
AppConfig.terminalFollowTheme: boolean
    ↓
Zustand store → useEffect → themeManager
    ↓
themeManager:
  - theme === "auto" → 监听 matchMedia('(prefers-color-scheme: light)')
  - 计算 resolvedTheme ("light" | "dark")
  - 设置 document.documentElement.dataset.theme = resolvedTheme
  - CSS transition 自动处理过渡动画
    ↓
CSS: :root (深色默认) / :root[data-theme="light"] (浅色覆盖)
    ↓
组件通过 var(--*) 自动响应
xterm 根据 resolvedTheme + terminalFollowTheme 选择色板
```

## 配色方案

### 深色 Warm Carbon（现有，不变）

| 变量 | 值 |
|------|-----|
| `--bg-base` | `#0e0d0b` |
| `--bg-surface` | `#161513` |
| `--bg-elevated` | `#1e1c19` |
| `--bg-overlay` | `#262421` |
| `--bg-terminal` | `#100f0d` |
| `--accent` | `#c8805a` |
| `--accent-muted` | `#c8805a33` |
| `--accent-subtle` | `#c8805a18` |
| `--text-primary` | `#e5e0d8` |
| `--text-secondary` | `#9a9488` |
| `--text-muted` | `#5c5850` |
| `--border-subtle` | `rgba(255,255,255,0.05)` |
| `--border-default` | `rgba(255,255,255,0.08)` |
| `--border-strong` | `rgba(255,255,255,0.12)` |
| `--color-success` | `#6bb87a` |
| `--color-warning` | `#d4a84a` |
| `--color-error` | `#d4605a` |
| `--color-ai` | `#b08cd4` |
| `--color-file` | `#7dcfb8` |
| `--color-folder` | `#d4c8a0` |

### 浅色 Pure White（新增）

| 变量 | 值 |
|------|-----|
| `--bg-base` | `#ffffff` |
| `--bg-surface` | `#f5f5f5` |
| `--bg-elevated` | `#ebebeb` |
| `--bg-overlay` | `#e0e0e0` |
| `--bg-terminal` | `#fafafa` |
| `--accent` | `#b06830` |
| `--accent-muted` | `#b0683033` |
| `--accent-subtle` | `#b0683018` |
| `--text-primary` | `#1a1a1a` |
| `--text-secondary` | `#666666` |
| `--text-muted` | `#999999` |
| `--border-subtle` | `rgba(0,0,0,0.06)` |
| `--border-default` | `rgba(0,0,0,0.10)` |
| `--border-strong` | `rgba(0,0,0,0.15)` |
| `--color-success` | `#2d8a46` |
| `--color-warning` | `#b08620` |
| `--color-error` | `#c0392b` |
| `--color-ai` | `#8a5cb8` |
| `--color-file` | `#1a8a6a` |
| `--color-folder` | `#8a7a40` |

### xterm 浅色终端色板

仅在 `terminalFollowTheme === true` 且 `resolvedTheme === "light"` 时使用：

| 属性 | 值 |
|------|-----|
| background | `#fafafa` |
| foreground | `#1a1a1a` |
| cursor | `#b06830` |
| cursorAccent | `#fafafa` |
| selectionBackground | `#b0683030` |
| selectionForeground | `#1a1a1a` |
| black | `#1a1a1a` |
| red | `#c0392b` |
| green | `#2d8a46` |
| yellow | `#b08620` |
| blue | `#2860a0` |
| magenta | `#8a5cb8` |
| cyan | `#1a8a6a` |
| white | `#f0f0f0` |
| brightBlack | `#666666` |
| brightRed | `#e04030` |
| brightGreen | `#38a058` |
| brightYellow | `#c89830` |
| brightBlue | `#3870b8` |
| brightMagenta | `#a070d0` |
| brightCyan | `#28a080` |
| brightWhite | `#ffffff` |

## 文件变更清单

### Rust 后端

**`src-tauri/src/config.rs`**
- `AppConfig` 结构体新增字段：
  - `theme: String` — 默认 `"auto"`，可选 `"light"` / `"dark"`
  - `terminal_follow_theme: bool` — 默认 `true`

### CSS

**`src/styles.css`**
- `:root` 添加 `transition` 属性用于平滑过渡
- 新增 `:root[data-theme="light"] { ... }` 块，覆盖所有 CSS 变量为浅色值
- 检查并调整滚动条、上下文菜单、prompt 对话框等在浅色下的表现

### TypeScript 类型

**`src/types.ts`**
- `AppConfig` 接口新增：
  - `theme: 'auto' | 'light' | 'dark'`
  - `terminalFollowTheme: boolean`

### 新文件

**`src/utils/themeManager.ts`**
- `applyTheme(theme, terminalFollowTheme)` — 主入口
- 内部监听 `matchMedia('(prefers-color-scheme: light)')` 的 `change` 事件
- 计算 `resolvedTheme` 并设置 `document.documentElement.dataset.theme`
- 导出 `getResolvedTheme()` 供终端和其他组件查询当前实际主题
- 返回清理函数用于解除监听

### Store

**`src/store.ts`**
- 在 config 初始化和变更时调用 themeManager

### 终端

**`src/utils/terminalCache.ts`**
- 导出 `DARK_TERMINAL_THEME` 和 `LIGHT_TERMINAL_THEME` 两个对象
- `getOrCreateTerminal()` 根据当前 resolvedTheme + terminalFollowTheme 选择色板

**`src/components/TerminalInstance.tsx`**
- 监听主题变更，动态更新已有终端实例的 `terminal.options.theme`
- 移除硬编码 `bg-[#100f0d]`，改用 `bg-[var(--bg-terminal)]`

### 设置面板

**`src/components/TerminalConfigModal.tsx`**
- 新增"主题模式"区域：三选一按钮（深色 / 浅色 / 跟随系统）
- 新增"终端跟随主题"开关

### 组件硬编码修复

**`src/components/StatusDot.tsx`**
- 状态颜色改用 CSS 变量（`var(--text-muted)`, `var(--color-success)`, `var(--color-ai)`, `var(--color-error)`）

**`src/components/FileTree.tsx`**
- Git 状态颜色用 CSS 变量替代 Tailwind 固定色类（`text-[var(--color-warning)]` 等）

**`src/components/SessionList.tsx`**
- AI 类型徽标颜色改用 CSS 变量

**`src/components/CommitDiffModal.tsx`**
- diff 状态标签颜色改用 CSS 变量

## 过渡动画

在 `:root` 上添加：

```css
:root {
  transition: background-color 0.25s ease, color 0.25s ease;
}

:root *,
:root *::before,
:root *::after {
  transition: background-color 0.25s ease, color 0.25s ease, border-color 0.25s ease, box-shadow 0.25s ease;
}
```

## 边界情况

- **噪声纹理叠加**（`styles.css` 中的 SVG noise）：浅色下需降低不透明度或调整混合模式
- **毛玻璃效果**（上下文菜单 `backdrop-filter`）：浅色下 `background` 需改为半透明白
- **Allotment 分割线**：需在浅色下可见（当前是 `rgba(255,255,255,...)` 形式）
- **系统主题变更**：用户在 Windows 设置中切换深浅时，应用实时响应（`matchMedia` change 事件）
- **应用启动时**：从 config.json 读取 theme 后，在首次渲染前应用，避免闪烁

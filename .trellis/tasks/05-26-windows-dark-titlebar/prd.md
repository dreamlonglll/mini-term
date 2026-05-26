# Windows 深色模式标题栏跟随切换 (issue #33)

## Goal

修复 Windows 深色模式下原生标题栏仍为浅色与内容区深色撞色的视觉割裂问题，让标题栏配色跟随应用 theme（auto/light/dark）切换，对齐 Windows Terminal 默认观感。

## Background

- Issue #33 (sungnix)：用户反馈 Windows 深色模式下标题栏浅色突兀，希望统一深色或像 Windows Terminal 默认深色标题栏。
- 当前 `tauri.conf.json` 未关闭 `decorations`，沿用 Windows 原生系统标题栏；前端 `themeManager` 切换只动 webview 内部，标题栏配色不变。

## What I already know

- 项目用 Tauri v2 + 系统原生标题栏（`tauri.conf.json` 未设 `decorations: false`）。
- 已有 `src/utils/themeManager.ts`，统一 `applyToDOM` 出口，可挂钩。
- `src-tauri/Cargo.toml` 已有 `windows = "0.58"` crate，缺 `Win32_Graphics_Dwm` feature。
- 平台支持现状（来自 memory）：Windows 主要支持，macOS/Linux 代码支持但可用性欠佳，所以非 Windows 不需要等价实现。

## Technical Approach

**方案 A（已选定）**：调 Win32 `DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE=20, &dark, 4)` 切换原生标题栏深浅模式。

实施点位：
1. `src-tauri/Cargo.toml` 给 Windows target 的 `windows` crate 加 `Win32_Graphics_Dwm` feature。
2. `src-tauri/src/` 新增 `set_window_dark_mode(window: Window, dark: bool)` Tauri command，从 window 取 HWND 调 `DwmSetWindowAttribute`。
   - 非 Windows 平台 `#[cfg(target_os = "windows")]` 包裹，其它平台 command 直接返回 `Ok(())` 做 no-op。
3. `lib.rs` 注册 command。
4. 前端 `themeManager.applyToDOM` 内调 `invoke('set_window_dark_mode', { dark: theme === 'dark' })`。
5. 启动时根据 localStorage 持久化的 theme 立即调用一次，避免首帧浅色闪烁。

## Requirements (MVP)

- [ ] Windows 11 / Windows 10 2004+ 下，深色 theme 时标题栏切深色，浅色 theme 时标题栏切浅色。
- [ ] `auto` 模式跟随系统 `prefers-color-scheme` 自动切换。
- [ ] 启动时立即应用，无首帧闪烁。
- [ ] 非 Windows 平台 command no-op，不报错不影响功能。

## Open Questions

（已全部解决，见下方 Decision）

## Acceptance Criteria

- [ ] Windows 下手动切 light → dark → auto，标题栏配色跟随变化（含最小化最大化按钮颜色反转）。
- [ ] 重启应用后标题栏配色与持久化 theme 一致，无浅色闪烁。
- [ ] macOS/Linux 构建通过、运行正常，无 panic。
- [ ] `cargo build` Windows target 通过。

## Definition of Done

- 代码改动符合 spec
- Windows 实测三种 theme（light/dark/auto）切换效果
- macOS/Linux 至少保证 `cargo build` 通过
- 不需新增测试（GUI 行为，单元测试覆盖意义有限；可加 build_wslenv_value 风格的纯函数单测如果有可测点）
- 不需更新 README（视觉修复，不改外部接口）

## Out of Scope

- 不做自绘标题栏（方案 B 范围）。
- 不改窗口控件颜色/字体（系统决定）。
- 不做 macOS `vibrancy` / Linux GTK header bar 等价适配。
- 不做 Windows 7/8 兼容（Tauri v2 本身不支持）。

## Technical Notes

- DWM 属性参考：`DWMWA_USE_IMMERSIVE_DARK_MODE_BEFORE_20H1 = 19`（1809-1909），`DWMWA_USE_IMMERSIVE_DARK_MODE = 20`（20H1+）
- `windows` crate 0.58 的 Dwm 函数：`windows::Win32::Graphics::Dwm::DwmSetWindowAttribute`
- Tauri v2 取 HWND：`window.hwnd()?` 返回 `HWND`
- 钩子点：`src/utils/themeManager.ts:51` `applyToDOM` 函数

## Decision (ADR-lite)

**Context**: Windows 原生标题栏与 webview 深色主题撞色，issue #33 报告。

**Decision**:
- 走方案 A（`DwmSetWindowAttribute`），保留原生窗口控件，仅切配色。
- 只用 `DWMWA_USE_IMMERSIVE_DARK_MODE = 20`（Win10 20H2 / 21H1+ 和 Win11 全支持），不做 attr=19 fallback。
- 调用失败时 `eprintln!` 一条警告（含 HRESULT），不 panic，不阻塞主功能。

**Consequences**:
- ✅ 改动小、风险低、保留所有原生窗口行为（snap/双击/Alt+Space）
- ✅ 标题栏文字色由系统接管，无需自维护
- ✅ 失败有日志可追溯
- ❌ Win10 1809-1909 标题栏仍为浅色（用户群极少，可接受）
- ❌ 标题栏 hover 高亮色仍为系统决定，无法定制

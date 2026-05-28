# xterm 终端支持连体字 (ligatures)

## Goal

让 mini-term 终端能渲染编程字体的连体字（如 Fira Code 的 `==`、`=>`、`!=`、`->`、JetBrains Mono 的 `===` 等），
当前虽然「设置 → 字体 → 终端字体」可填字体名，但 xterm.js 在 WebGL 渲染器下逐字符纹理化，
即使填入 `FiraCode Nerd Font` 也不会合成 ligature glyph，用户体验上「字体选项形同虚设」。

## What I already know

- 当前 xterm 在 `src/utils/terminalCache.ts:330` 通过 `activateWebgl(ptyId)` 加载 `WebglAddon`。
- 现有配置项 `terminalFontFamily`（Rust `terminal_font_family: Option<String>`，TS `terminalFontFamily?: string`）已贯通三层：`config.rs` ↔ `types.ts` ↔ `SettingsModal.tsx`。
- 默认字体链 `DEFAULT_TERMINAL_FONT_FAMILY` 已含 `JetBrainsMono Nerd Font` / `Cascadia Code`，本身带 ligature。
- `@xterm/addon-ligatures@0.10.0` 最新 stable，**未声明 peerDependencies**，与 `@xterm/xterm@^6.0.0` 实测兼容。
- **重大修正**（来自 [`research/xterm-ligatures.md`](research/xterm-ligatures.md)）：
  - 官方 README 过时，addon `0.10.0` 实际不再依赖 Node `font-finder`，改用浏览器 `window.queryLocalFonts()` API。
  - **WebGL + LigaturesAddon 官方支持共存**，无需放弃 WebGL 加速。官方 demo 也是 WebGL + Ligatures 同时启用。
  - 启用顺序：先 `loadAddon(new LigaturesAddon())`，再 `loadAddon(new WebglAddon())`；运行时切换需 dispose WebGL → load ligatures → 新建 WebGL。
- **平台差异**：
  - **Windows / WebView2 (Chromium 103+)** → `queryLocalFonts` 可用，真实解析磁盘字体的 GSUB `calt` 表，完整 ligature 支持。
  - **macOS / WKWebView、Linux / WebKitGTK** → `queryLocalFonts` 缺失，addon 自动 fallback 到硬编码的 60 条 Iosevka `calt` 列表（覆盖常见 `==`、`!=`、`->`、`=>` 等编程连字，但不是真完整）。
- 首次调用 `queryLocalFonts` 时 WebView2 是否弹权限确认，Tauri 行为未明，需上机验证。

## Assumptions (temporary)

- 用户主要在 Windows 上使用（与 [项目平台支持现状](../../../../C:/Users/12197/.claude/projects/D--Git-mini-term/memory/project_platform_support.md) 一致），macOS/Linux 的 60 条 Iosevka fallback 是「锦上添花」级别。
- 现有终端的 xterm 实例会按 ptyId 缓存复用（见 `terminalCache.ts` 顶部注释），切换开关时需要支持「热更新到已开终端」而非要求用户重新开 tab。
- 配置项默认值需要决策：**开** vs **关**。

## Open Questions

- (Q3, Preference, MVP scope) **运行时切换的范围**：切换开关后，是否要立刻应用到所有「已经打开」的终端 pane？还是仅对新建 pane 生效？

## Requirements (evolving)

- [ ] 新增配置项 `terminal_ligatures: bool` / `terminalLigatures: boolean`（Rust + TS），**默认 `false`**（用户主动开启）。
- [ ] 设置面板「字体」页在终端字体输入框下方新增「启用连体字」开关。
- [ ] `getOrCreateTerminal` 在创建终端时，按配置加载 `LigaturesAddon`，保持 WebGL 加速。
- [ ] 加载顺序遵循官方 demo：先 `loadAddon(new LigaturesAddon())`，再 `activateWebgl`。
- [ ] 全平台一视同仁不在 UI 区分平台差异（macOS/Linux 上 addon 自动 fallback 到内置 Iosevka 60 条 calt，不报错），仅在 README 提一句平台差异。

## Acceptance Criteria (evolving)

- [ ] Windows + JetBrains Mono / Fira Code 字体下，在终端键入 `=>` `===` `!=` `->` 可看到 ligature glyph。
- [ ] 关闭开关后立即（或新建 pane 后）回到逐字符显示，不再合成 ligature。
- [ ] 启用 ligatures 不破坏 WebGL 加速，重度滚屏场景帧率与当前持平。
- [ ] 设置项 round-trip 持久化：写入 config.json → 重启 app → 仍为期望状态。
- [ ] 旧 config.json（无 `terminalLigatures` 字段）能正常加载，默认值生效。

## Definition of Done (team quality bar)

- Rust `cargo test` 通过（含 `terminal_ligatures` round-trip 单测）。
- TS `npm run build` 编译通过，无 type 错误。
- Windows 上人工验证开/关切换行为符合 Acceptance Criteria。
- README「外观与配置」段补充连体字开关说明（含 Win 完整支持 / macOS·Linux Iosevka fallback 限制）。
- 版本号同步升级（package.json + Cargo.toml + tauri.conf.json + Cargo.lock），按 [发版工作流](../../../../C:/Users/12197/.claude/projects/D--Git-mini-term/memory/feedback_release_workflow.md) 走。

## Out of Scope (explicit)

- 不自带/打包额外字体；用户需自己装 Fira Code 等 ligature 字体。
- 不为 macOS/Linux 寻找 `queryLocalFonts` 的 polyfill，沿用 addon 内置 fallback。
- 不解决 xterm.js 已知 WebGL+ligature 边缘 bug（#3303 paint 残留、#4000 选区错位、#4362 极长连字死循环）——属上游问题，仅在文档中提及。
- 不为「按 pane 独立配置 ligatures」做支持；全局一个开关。

## Research References

- [`research/xterm-ligatures.md`](research/xterm-ligatures.md) — addon 0.10.0 实际用 `queryLocalFonts`，WebGL 共存官方支持，平台差异显著（Win 完整 / mac·Linux 60 条 Iosevka fallback）。

## Technical Notes

- 改动点（5 处协同）：
  1. `src-tauri/src/config.rs` — `AppConfig` 加 `terminal_ligatures: bool`，`#[serde(default)]` 兼容旧 config。
  2. `src/types.ts` — `AppConfig` 加 `terminalLigatures?: boolean`。
  3. `src/store.ts` — 默认值。
  4. `src/utils/terminalCache.ts` — 在 `getOrCreateTerminal` 中按配置 `loadAddon(new LigaturesAddon())`；在 `activateWebgl` 顺序上确保 ligatures 先于 webgl；新增「切换 ligatures」函数（dispose webgl → load/unload ligatures → reload webgl）供配置 watcher 调用。
  5. `src/components/SettingsModal.tsx` — 字体页加开关 + 切换时调 watcher。
- 包：`npm i @xterm/addon-ligatures@^0.10.0`（与现有 `@xterm/xterm@^6.0.0` 兼容）。
- 测试矩阵：
  - Windows 11 + WebView2 + Fira Code → 期望真 ligature。
  - Windows 11 + 无 Fira Code（只默认链 JetBrains Mono）→ JetBrains Mono 本身有 calt，期望真 ligature。
  - 不能上手测的 macOS/Linux：仅靠 addon fallback 列表，文档说明限制。

## 更新内容

### 新增

- **Fluent 2 皮肤** — 设置 → 外观新增「Fluent 2」选项，整体切到 Windows 11 风格视觉：柔和渐变背景配双层 radial-gradient「天光」、亚克力玻璃面板（`backdrop-filter: blur(24px) saturate(140%)`）、Fluent 蓝主色（深色 `#4cc2ff` / 浅色 `#0067c0`）、清晰圆角与高光描边。File tree hover、button hover、scrollbar、Allotment 分隔条 hover 全套带 Fluent 缓动曲线（`cubic-bezier(0.33, 0, 0.67, 1)`，180ms）。深/浅主题各自独立色板，终端配色随「终端跟随主题」切到 Fluent 专属色板。

### 修复

- **Fluent 2 下右键 Session / Git 查看弹窗位置异常** — 启用 Fluent 2 后右键 Session 列表项「查看」、右键 Git 提交「查看」弹出的 modal 不再页面正中铺满，而是「挤」在对应面板的内容区域里，看起来像窗口被截断了一半。根因：Fluent 2 给 `[data-panel]` 加 `backdrop-filter: blur(...)` 实现亚克力玻璃，而 CSS 规范规定祖先有 `backdrop-filter` / `transform` / `filter` / `perspective` / `will-change` / `contain` 等属性时即成为后代 `position: fixed` 的 containing block —— modal 的 `fixed inset-0` 不再贴 viewport 而是贴面板边界。本版把挂在面板子树内的 7 个 modal（SessionViewer / CommitDiff / FileViewer / Diff / ProjectEnvVars / SshAssoc + ProjectList 内联删除确认）统一改用 `createPortal(jsx, document.body)` 渲染到 body 子节点，彻底绕开任何祖先的 containing block 拽拉，以后任何主题在祖先链加 containing-block-触发属性都不会再复现。

- **Fluent 2 亚克力面板内嵌 SessionList / GitHistoryContent 子容器实色背景破坏玻璃感** — Fluent 2 下 SessionList 与 Git 历史内容区子容器自带 `bg-[var(--bg-surface)]` 实色，叠在半透明亚克力面板内部呈现「面板里又套了一块深色矩形」，破坏整体玻璃感。本版加 `[data-skin="fluent2"] [data-panel] .bg-[var(--bg-surface)]:not(.sticky)` 透明覆盖让子容器继承面板亚克力底色，保留 GitHistoryContent sticky 表头实色避免滚动穿透。

- **v0.4.19 atlas glyph 错位乱码 `_isPaused` 拦截路径兜底** — v0.4.19 修复多 Claude Code 终端共享 atlas 同时乱码后，仍偶发单行/部分行换字型乱码（中文/数字变拉丁字母组合），切走再切回 tab 时部分行不自愈，必须 resize 才能恢复。根因：xterm.js core RenderService 自带 IntersectionObserver 监视 `screenElement`，wrapper.remove 后 `_isPaused=true`，atlas 事件路径的 `term.refresh` 全被 `refreshRows` 吞掉只设 `_needsFullRefresh`；切回 tab 时虽自动 flush 一次 refreshRows，但 partial `_updateModel(start, end)` 不覆盖含旧 glyph 坐标的行 → 表现为单行/部分行换字、不会自愈。本版在 TerminalInstance mount 第三层 rAF + visibilityObserver intersecting 回调内调 `clearAtlasForPty(ptyId)`（内部走 `webglAddon.clearTextureAtlas()` + 全行 `_clearModel` / `_updateModel(0, rows-1)` 重写所有 cell glyph 坐标），绕开 `_isPaused` 期间被吞的 `term.refresh` 残留；事件路径保留 `term.refresh` 不切 `clearTextureAtlas`，避免可见终端因 vertex buffer / lineLengths 全 fill(0) 出现 < 1 帧空白闪烁。新增可开关诊断日志 `localStorage.miniterm.atlasDebug='1'` 触发 `atlas-event` / `clear-atlas` console.log（含 reason / cacheSize / 各终端 isPaused 反射读 RenderService 私有字段），默认 OFF 不污染普通用户控制台。

## 下载

- **Windows** — `Mini-Term_0.4.20_x64-setup.exe`（NSIS，推荐）或 `Mini-Term_0.4.20_x64_en-US.msi`
- **macOS (Apple Silicon)** — `Mini-Term_0.4.20_aarch64.dmg`（首次启动需在终端运行 `xattr -cr /Applications/Mini-Term.app` 解除 Gatekeeper quarantine）
- **Linux** — `Mini-Term_0.4.20_amd64.AppImage` / `Mini-Term_0.4.20_amd64.deb` / `Mini-Term-0.4.20-1.x86_64.rpm`

> 平台支持：Windows 为主要支持平台，日常开发与测试均在 Windows 上进行；macOS / Linux 代码层面已支持，但未经充分打磨，可用性欠佳，欢迎提 Issue 反馈。

---

**完整 commits**: https://github.com/dreamlonglll/mini-term/compare/v0.4.19...v0.4.20

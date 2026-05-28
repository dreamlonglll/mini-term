# xterm.js LigaturesAddon 与 WebglAddon 的加载顺序、热切换与平台差异

> `@xterm/addon-ligatures@0.10.0` 必须在 `@xterm/addon-webgl@0.19.x` 之前 `loadAddon`，否则 `font-feature-settings` 不会进 WebGL 纹理 atlas（上游 [xtermjs/xterm.js#5455](https://github.com/xterm-js/xterm.js/issues/5455)）；切换 ligatures 或字体后必须按 `dispose webgl → dispose ligatures → reload ligatures → reload webgl` **同步无 await** 重做这条链路，否则 pty-output 全局监听器会在 dispose 与 reload 之间 race。

---

## What

任何在本项目中使用 xterm.js + `@xterm/addon-webgl` + `@xterm/addon-ligatures` 的代码，**必须**满足以下三条：

### 约束 1：加载顺序

```typescript
// ✅ 正确：ligatures 在 webgl 之前
term.loadAddon(new LigaturesAddon());
term.loadAddon(new WebglAddon());
```

```typescript
// ❌ 错误：webgl 先加载，ligature glyph 永远不会进 atlas
term.loadAddon(new WebglAddon());
term.loadAddon(new LigaturesAddon());
```

`activateWebgl` / 任何 webgl 重建路径都必须先调一遍 `loadLigaturesIfEnabled(entry)`。

### 约束 2：切换 ligatures / 字体的热更新链路

```typescript
// ✅ 正确：纯同步，无 await
export function reloadLigaturesForPty(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry || !entry.webglLoaded) return;
  disposeWebgl(entry);
  disposeLigatures(entry);
  loadLigaturesIfEnabled(entry);
  loadWebgl(entry);
  entry.term.refresh(0, entry.term.rows - 1);
}
```

```typescript
// ❌ 错误：任何 await 都会让 pty-output listener 在 dispose 后、reload 前写入已 dispose 的 webgl renderer
export async function reloadLigaturesForPty(ptyId: number): Promise<void> {
  disposeWebgl(entry);
  await someAsyncSetup();  // ← race window
  loadWebgl(entry);
}
```

约束细节：
- `mount` 流程尚未完成（`webglLoaded === false`）时**必须早返回** —— 让首次 `activateWebgl` 自然按当前配置加载，避免 mount 中途双重 load。
- 切换字体时也要触发此函数（不只是 toggle ligatures），因为上游 #5455：`font-feature-settings` 变化不会通知 WebGL atlas 重建。

### 约束 3：`new Terminal()` 必须开 `allowProposedApi: true`

```typescript
// ✅ 正确
const term = new Terminal({
  fontFamily: '...',
  allowProposedApi: true,  // ← LigaturesAddon 需要
});
```

```typescript
// ❌ 错误：addon 加载时抛 "You must set the allowProposedApi option to true to use proposed API"
const term = new Terminal({ fontFamily: '...' });
term.loadAddon(new LigaturesAddon());  // 抛错,被 try/catch 吞掉,表现为开关无效果
```

`LigaturesAddon` 内部调用 `term.registerCharacterJoiner` 注入字符合并逻辑,而 `registerCharacterJoiner` 在 xterm.js v6 中仍标记为 **proposed API**(未来可能调整签名),默认禁用。必须在 `new Terminal()` 配置中显式 opt-in。

注意:这个错误会被 `loadLigaturesIfEnabled` 内的 `try/catch` 静默吞掉(只 console.error),表现为开关切到 ON 但终端无任何视觉变化 —— 排查时必须看 devtools console 才能定位。

### 约束 4：禁止依赖 README 中的 `font-finder` / Node `fs` 描述

`@xterm/addon-ligatures@0.10.0` 的官方 README 仍声明依赖 Node `font-finder` 走 Node `fs`。**这个说明已过时**，published `lib/addon-ligatures.mjs` 实际使用浏览器原生 [`window.queryLocalFonts()`](https://developer.mozilla.org/en-US/docs/Web/API/Window/queryLocalFonts)，无 Node `fs`、无 WASM、无 `eval`。

实践影响：
- **不要**为这个 addon 改 Tauri CSP / capability 让 Node `fs` 可用（不需要）。
- **不要**为这个 addon 配 webpack/vite externals（bundle 已是浏览器 mjs）。
- `package.json` 里残留的 `font-finder` / `font-ligatures` transitive deps 是 upstream 没清，Vite tree-shake 可移除，不影响运行。

---

## Why

### 为什么 ligatures 必须先于 webgl（约束 1）

xterm.js WebGL renderer 的纹理 atlas 在第一次 `loadAddon(new WebglAddon())` 时就根据当时的 `term.options.fontFamily` + `font-feature-settings` 栅格化所有 glyph。LigaturesAddon 工作方式是注入 `font-feature-settings: "calt"` + 解析字体 GSUB calt 表把 `==` 拆/拼成 ligature glyph。如果 WebGL 先 load，atlas 已经按"无 calt"的字符宽度建好，后注入的 ligature glyph 找不到位置——上游 #5455 的核心症状。

### 为什么热切换不能 await（约束 2）

`terminalCache.ts` 顶部的 `ensureGlobalPtyOutputListener` 注册了一个全局 `pty-output` listener，所有终端的 PTY 输出由它分发到对应 `entry.term.write(data)`。`term.write` 触发渲染，渲染走当前 loaded renderer。

如果 reload 函数中间有 `await`：
1. dispose webgl 后 entry.term 退回 DOM renderer（xterm.js 自动 fallback）
2. await 期间，pty-output listener 仍会写入 term —— 这部分数据走 DOM renderer 渲染
3. await 之后 load webgl 重建 atlas，但 DOM renderer 上的 dirty 状态可能没被新 webgl renderer 接管

具体不是数据丢失，是 atlas/vertex buffer 状态不一致导致的乱码。同步无 await 保证整条链在 JS event loop 单帧内完成，listener 没机会插队。

### 为什么 README 关于 Node 依赖的描述错了（约束 3）

`@xterm/addon-ligatures@0.10.0` 的 npm tarball 解包：`lib/addon-ligatures.mjs` 是浏览器 bundle，使用 `window.queryLocalFonts` API（Chromium 103+ Local Font Access API）查询用户系统字体并解析其 calt 表。Node `font-finder` 仅在 `package.json` 的 `dependencies` 里残留，bundle 中没有 import。

---

## 平台差异（必读）

| 平台 | WebView | `window.queryLocalFonts` | LigaturesAddon 实际行为 |
|------|---------|--------------------------|------------------------|
| Windows | WebView2 (Chromium 103+) | ✅ 可用 | 真实解析磁盘字体 GSUB calt 表，完整 ligature 支持 |
| macOS | WKWebView | ❌ 不支持 | 静默 fallback 到内置硬编码的 ~60 条 Iosevka calt（覆盖 `==` `!=` `->` `=>` 等常见编程连字） |
| Linux | WebKitGTK | ❌ 不支持 | 同上 |

**Tauri-side**：首次调用 `queryLocalFonts` 时 WebView2 是否弹原生权限对话框，行为未明文档化；若弹出且用户拒绝，addon 自动走 fallback 列表，不影响 app 稳定性。

UI 设计含义：**不要**在前端检测平台后禁用「启用连体字」开关 —— 即便 mac/Linux 也有 60 条 fallback 可用，用户期望开关行为一致。但 README / 设置面板说明里**必须**注明平台差异。

---

## Good / Base / Bad Cases

### Good

- 新建终端时 `activateWebgl(ptyId)` 内先调 `loadLigaturesIfEnabled` 再 `loadWebgl`，配置为 false 时跳过 ligatures load。
- 用户切换「启用连体字」开关 / 终端字体 → `useEffect([terminalLigatures, terminalFontFamily, ptyId])` 触发 `reloadLigaturesForPty(ptyId)` 同步重做链路。
- 配置切换的同一帧内 ligatures + webgl 状态原子地翻转，无中间态可见。

### Base

- mount 流程中 ligatures effect 同步触发但 `webglLoaded === false` → 早返回，让 RAF 内的 `activateWebgl` 按 mount 时配置一次性加载。
- 用户填了一个不存在的字体名 → addon 不报错，视觉上无 ligature 效果，符合预期。
- mac/Linux 用户开了开关 → 自动用内置 60 条 Iosevka fallback，UI 不区分平台。

### Bad

- 在 `loadWebgl` 之后才调 `loadLigaturesIfEnabled` → ligatures 看起来"加载成功"但 WebGL atlas 不含 ligature glyph，视觉上无效果。
- `reloadLigaturesForPty` 内部插入 `await invoke('save_config', ...)` 等任意异步调用 → pty-output 在 race 窗口内写入，触发乱码（与共享 atlas merge 同症状）。
- 字体切换时只更新 `term.options.fontFamily` 不调 `reloadLigaturesForPty` → WebGL atlas 仍用旧字体 glyph，ligature 不跟随新字体的 calt 表。

---

## Tests Required

- **Rust round-trip**（`config.rs::tests`）：`terminal_ligatures` 字段在 camelCase JSON ↔ struct 之间双向序列化保持；旧 config.json（无该字段）反序列化默认 `false`。
- **手动验证**（无法自动化，因 Tauri webview 不能在 CI 中拉起）：
  1. Windows + Fira Code / JetBrains Mono：终端键入 `==` `=>` `!=` `->`，开关 ON 应显示 ligature glyph，OFF 应回到分字符。
  2. 切换开关后，**已开终端 pane** 立即生效（无需关 tab 重开）。
  3. 字体输入框改成新 ligature 字体后，已开终端 ligature 跟随新字体（验证字体切换也触发 reload 链路）。
  4. WebGL 仍生效（任务管理器 GPU 利用率 > 0，重度滚屏帧率与 ligatures 关闭时持平）。

---

## Wrong vs Correct

### Wrong：把 ligatures 当成一个独立 addon 加载，跟 webgl 顺序无关

```typescript
// ❌ 想"先把 webgl 调好，再 anytime 叠加 ligatures"
export function activateWebgl(ptyId: number) {
  loadWebgl(entry);
}

export function enableLigatures(ptyId: number) {
  loadLigaturesIfEnabled(entry);  // ← 此时 webgl atlas 已用旧 font-feature-settings 建好
}
```

### Correct：把 ligatures + webgl 当成"必须共生 + 必须顺序"的一对，任何状态变化都重做整条链

```typescript
export function activateWebgl(ptyId: number) {
  loadLigaturesIfEnabled(entry);  // ← 必须先
  loadWebgl(entry);
}

export function reloadLigaturesForPty(ptyId: number) {
  disposeWebgl(entry);
  disposeLigatures(entry);
  loadLigaturesIfEnabled(entry);  // 重新读 config
  loadWebgl(entry);                // 重建 atlas
  entry.term.refresh(0, entry.term.rows - 1);
}
```

---

## 相关

- 实现：`src/utils/terminalCache.ts`（`loadLigaturesIfEnabled` / `loadWebgl` / `activateWebgl` / `reloadLigaturesForPty`）+ `src/components/TerminalInstance.tsx`（`useEffect([terminalLigatures, terminalFontFamily, ptyId])`）
- 任务 PRD：`.trellis/tasks/05-28-xterm-ligatures-support/prd.md`
- 研究：`.trellis/tasks/05-28-xterm-ligatures-support/research/xterm-ligatures.md`
- 上游 issue：[xtermjs/xterm.js#5455](https://github.com/xterm-js/xterm.js/issues/5455)（font-feature-settings 不进 WebGL atlas）

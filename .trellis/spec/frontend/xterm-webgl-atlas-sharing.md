# xterm.js WebGL TextureAtlas 跨实例共享:atlas 变更后必须唤醒所有终端

> `@xterm/addon-webgl@0.19.0` 的 `TextureAtlas` 不是 per-terminal 的 —— `CharAtlasCache` 按 (fontFamily, fontSize, fontWeight, theme colors, devicePixelRatio, deviceCellWidth, deviceCellHeight, deviceMaxTextureSize) 命中后,多个 `Terminal` 实例**共享同一个 atlas + 内部 glyph 对象池**。atlas page merge / overflow page 创建时,会**原地改写**所有相关 glyph 的 `texturePage / texturePosition.x / texturePosition.y / sizeClipSpace` 字段。各 Terminal 的 `GlyphRenderer` GPU vertex buffer 仍引用旧值;只有被 xterm.js core 重新 schedule `renderRows` 的 renderer 才会经 `beginFrame → _clearModel(true) + _updateModel(0, rows-1)` 重写 buffer。**dormant 的 renderer**(无 PTY 输出、无光标移动、无选区变化)永远不消费这次变更,vertex buffer 保留错位坐标,渲染出 atlas 上其他位置的字形 —— 表现为多个终端同时出现"换字"型乱码(中文变拉丁字母组合,同字必同乱)。

---

## Convention:加 WebglAddon 时必须挂 atlas 变更监听,广播 refresh 到所有 cache 内 terminal

`src/utils/terminalCache.ts` 是项目里**唯一**创建 `Terminal` 与 `WebglAddon` 的入口(经 `getOrCreateTerminal` 缓存 + `activateWebgl` / `loadWebgl` 激活)。在 `new WebglAddon()` 之后、`loadAddon(webgl)` 之前,必须挂两个监听:

```ts
function refreshAllTerminalsForAtlasChange(): void {
  for (const e of cache.values()) {
    if (e.term.rows > 0) e.term.refresh(0, e.term.rows - 1);
  }
}

// 在 activateWebgl / loadWebgl 内:
const webgl = new WebglAddon();
webgl.onContextLoss(() => { /* ... */ });
webgl.onAddTextureAtlasCanvas(refreshAllTerminalsForAtlasChange);     // ← 必加
webgl.onRemoveTextureAtlasCanvas(refreshAllTerminalsForAtlasChange);  // ← 必加
entry.term.loadAddon(webgl);
```

`term.refresh(start, end)` 仅是 dirty 标记 —— 让 xterm.js core 在下一帧 schedule `renderRows`,进而让 WebglRenderer 检查 `_glyphRenderer.beginFrame()`(即 atlas 的 `_requestClearModel`),触发完整重绘从最新 glyph 字段重写 vertex buffer。本身极轻量,对正在活跃渲染的终端无副作用(同帧 dirty 合并)。

### 触发场景

任何 WebglAddon 实例的 `onAddTextureAtlasCanvas` / `onRemoveTextureAtlasCanvas` 都来自被它持有的 atlas;因 atlas 是共享的,**任一终端触发的事件就代表所有共享终端的 vertex buffer 可能失效**。只需任一终端把事件接到广播函数即可(无需每个都接,但多接也无害)。

---

## Wrong vs Correct

### Wrong

```ts
// 只挂 onContextLoss,不挂 atlas 变更监听
function loadWebgl(entry: CachedEntry): void {
  const webgl = new WebglAddon();
  webgl.onContextLoss(() => {
    webgl.dispose();
    entry.term.refresh(0, entry.term.rows - 1);
  });
  entry.term.loadAddon(webgl);   // ← atlas page merge 后,本终端若 dormant 就持续乱码
}
```

bug 表现:多 claude code 并发跑一段时间,**所有**终端同时出现**完全相同形状**的乱码(中文 → 拉丁字母组合),resize 单个终端可恢复但其他不恢复。

### Correct

```ts
function loadWebgl(entry: CachedEntry): void {
  const webgl = new WebglAddon();
  webgl.onContextLoss(() => {
    webgl.dispose();
    entry.term.refresh(0, entry.term.rows - 1);
  });
  webgl.onAddTextureAtlasCanvas(refreshAllTerminalsForAtlasChange);     // ← 唤醒所有 dormant 终端
  webgl.onRemoveTextureAtlasCanvas(refreshAllTerminalsForAtlasChange);
  entry.term.loadAddon(webgl);
}
```

---

## Common Mistake:以为每个终端有自己的 atlas

### Symptom
- 给单个终端测试时一切正常,跑多个 AI 终端(claude/codex)并发一段时间后所有终端同时显示乱码;
- 乱码不是色块/雪花,而是"别的合法字形"(中文变成拉丁字母组合);
- 同一字符多次出现的乱码完全一致;
- resize 一个终端只修复一个。

### Cause
- 假设每个 `new Terminal()` 拥有独立的 WebGL 资源 —— **错**。
- `acquireTextureAtlas`(`node_modules/@xterm/addon-webgl/src/CharAtlasCache.ts`)按配置全局命中,mini-term 所有终端的 fontFamily/fontSize/lineHeight/theme/DPR 均一致 → 100% 共享同一个 `TextureAtlas` 实例。
- atlas page merge 时 `_mergePages` 与 `_deletePage`(`TextureAtlas.ts:207-244`)原地改 glyph 字段,但 GPU vertex buffer 是 per-renderer 的,不会自动同步。

### Fix / Prevention
- 在 `loadWebgl` 内挂 `onAddTextureAtlasCanvas` / `onRemoveTextureAtlasCanvas` → `refreshAllTerminalsForAtlasChange`(见上文 Convention);
- **不要**用"给每个终端配不同 fontFamily 字符串绕过共享"作为修复 —— 内存 N 倍、首屏抖动、且 atlas 仍有上游 page merge 行为,治标不治本;
- **不要**禁用 WebGL 退回 Canvas —— 分屏 + 高频 TUI 输出场景性能明显下降。

---

## Gotcha:`_requestClearModel` 上游永不重置 + dormant renderer 漏唤醒

> **Warning**: `@xterm/addon-webgl@0.19.0` 的 `TextureAtlas._requestClearModel` 一旦被置 true,在整个 addon 源码内**没有任何地方**赋回 false(grep `_requestClearModel\s*=` 仅 3 处:1 处 init=false、2 处 assign=true)。这意味着 atlas merge 后,所有 owner renderer 每帧都强制 `_clearModel(true) + _updateModel(0, rows-1)` —— 上游"过度保守但安全"的兜底,但前提是 renderer 必须被 xterm.js core 至少 schedule 一次 `renderRows`。AI 终端等待响应时长时间无 PTY 输出 → render loop dormant → 永远不消费 `_requestClearModel` → 持续乱码。
>
> 本项目通过广播 `term.refresh(0, rows-1)` 强制 dirty 标记,补上上游遗漏的 dormant 唤醒。**不要假设 xterm.js core 会自己处理**。

---

## 适用范围与升级注意

- 锁定版本:`@xterm/xterm@6.0.0` + `@xterm/addon-webgl@0.19.0`(见 `package.json`)。
- 升级 `@xterm/addon-webgl` 前需检查:
  - `CharAtlasCache.ts` 是否仍是模块级 `charAtlasCache: ITextureAtlasCacheEntry[] = []` 全局缓存;
  - `TextureAtlas.ts` 的 `_mergePages` / `_deletePage` 是否仍直接修改 glyph 字段(而非生成新 glyph 对象);
  - `WebglRenderer.renderRows` 是否仍依赖 dirty schedule 触发 `beginFrame`;
  - 若上游已修复 dormant renderer 漏唤醒(`_requestClearModel` 改为 per-renderer flag,或 atlas 主动通过 event 强制 schedule frame),可考虑去掉本项目的广播 refresh —— 但需要回归测试多 claude 并发场景。
- 验收测试点(无自动化):分屏开 4 个终端各跑 `claude code`,持续对话 10 分钟以上,观察是否再现"所有终端同时乱码";切 tab 后切回的终端首屏是否正常。

# 根因:xterm.js WebGL TextureAtlas 跨终端共享 + dormant renderer 漏唤醒

## TL;DR

`@xterm/addon-webgl@0.19.0` 内的 `TextureAtlas` 是按 (fontFamily, fontSize, theme, DPR) **跨 Terminal 实例共享**的。atlas page merge / overflow page 创建时会**原地改写**所有 glyph 对象的 `texturePage` 与 atlas 内坐标。各 Terminal 自己的 `GlyphRenderer` GPU vertex buffer 在过往帧已写入旧值,只有被 xterm.js core 重新 schedule 的 renderer 才会通过 `renderRows → beginFrame → _clearModel(true) + _updateModel(0, rows-1)` 拿到新 glyph 字段重写 vertex buffer。AI 终端在"等待响应"时长时间无 PTY 输出,render loop dormant,vertex buffer 保留旧值 → 渲染时 GPU sample atlas 上错误位置 → 看到别的字形(而不是色块/雪花)。

## 视觉证据特征(用户截图)

- 中文 → 拉丁字母组合: "等**待**" → "等 **UKI**"、"**大概**率" → "**LO51** 率";
- 同一字符多次出现乱码**完全相同**("等待 LLM 响应/等待用户输入" 两个"待"都变成 UKI);
- 乱码字带"重叠/双影"(`帧/恢` 相邻、`清空` 的"空");
- 行间距、底色、行号、光标位置全部正常。

→ 不是 GPU context loss(那种是雪花/黑屏),而是采样坐标错位到了 atlas 上另一个合法字形的位置。完全锁定 vertex buffer ↔ atlas glyph 字段不同步。

## 证据链

### 1. atlas 跨实例共享(`node_modules/@xterm/addon-webgl/src/CharAtlasCache.ts`)

```ts
// 第 20 行 — 模块级全局
const charAtlasCache: ITextureAtlasCacheEntry[] = [];

// 第 26-76 行 — acquireTextureAtlas
//   先按 terminal 找命中条目,config 相同直接复用 entry.atlas;
//   再按 config 全量遍历查找,命中即 entry.ownedBy.push(terminal); return entry.atlas;
//   都不命中才 new TextureAtlas(...)
```

配置 key 由 `generateConfig` 拼装,包含: `deviceCellWidth, deviceCellHeight, deviceCharWidth, deviceCharHeight, fontFamily, fontSize, fontWeight, fontWeightBold, allowTransparency, drawBoldTextInBrightColors, minimumContrastRatio, colors(ReadonlyColorSet), devicePixelRatio, deviceMaxTextureSize`。 

mini-term 中所有终端均通过 `getOrCreateTerminal` 创建,fontFamily/fontSize/theme/lineHeight 完全一致,DPR 相同,主机 GPU 相同 → **所有终端 100% 命中同一个 atlas 实例**。

### 2. atlas page merge / overflow page 原地改写共享 glyph(`TextureAtlas.ts`)

```ts
// _createNewPage 第 155-197 行 — page 数达上限触发 merge
if (TextureAtlas.maxAtlasPages && this._pages.length >= Math.max(4, TextureAtlas.maxAtlasPages)) {
  ...
  const mergedPage = this._mergePages(mergingPages, mergedPageIndex);
  for (let i = sortedMergingPagesIndexes.length - 1; i >= 0; i--) {
    this._deletePage(sortedMergingPagesIndexes[i]);
  }
  this.pages.push(mergedPage);
  this._requestClearModel = true;
  this._onAddTextureAtlasCanvas.fire(mergedPage.canvas);
}

// _mergePages 第 207-233 行 — 原地改 glyph
for (const g of p.glyphs) {
  g.texturePage = mergedPageIndex;        // 改 page 指针
  g.sizeClipSpace.x = g.size.x / mergedSize;
  g.sizeClipSpace.y = g.size.y / mergedSize;
  g.texturePosition.x += xOffset;          // 改 atlas 内坐标
  g.texturePosition.y += yOffset;
  g.texturePositionClipSpace.x = g.texturePosition.x / mergedSize;
  g.texturePositionClipSpace.y = g.texturePosition.y / mergedSize;
}

// _deletePage 第 235-244 行 — 后续 page index 左移
this._pages.splice(pageIndex, 1);
for (let j = pageIndex; j < this._pages.length; j++) {
  for (const g of this._pages[j].glyphs) {
    g.texturePage--;
  }
}

// 第 785-803 行 — overflow page 创建分支同样会 fire onAddTextureAtlasCanvas + 置 _requestClearModel
```

`IRasterizedGlyph` 对象的所有权在 atlas,所有共享 atlas 的 renderer 通过指针读取。改字段 = 全员立刻可见。

### 3. vertex buffer 写入时机(`GlyphRenderer.ts`)

```ts
// 第 257-260 行 — _updateModel 时把 glyph 字段写进 vertex array
array[$i + 5] = $glyph.texturePositionClipSpace.x + $clippedPixels / this._atlas.pages[$glyph.texturePage].canvas.width;
array[$i + 7] = $glyph.sizeClipSpace.x       - $clippedPixels / this._atlas.pages[$glyph.texturePage].canvas.width;
```

每次 `_updateModel(start, end)` 才从 glyph 对象读字段写 GPU buffer。**buffer 写入后不会自动同步 atlas 后续的字段变更**。

### 4. dirty 兜底依赖 schedule(`WebglRenderer.ts`)

```ts
// 第 323-360 行 — renderRows 主路径
public renderRows(start: number, end: number): void {
  if (!this._isAttached) { ... }

  // 第 346-352 行 — atlas page merge 兜底
  if (this._glyphRenderer.value.beginFrame()) {       // ← 即 atlas._requestClearModel
    this._clearModel(true);
    this._updateModel(0, this._terminal.rows - 1);    // ← 全量重写 vertex buffer
  } else {
    this._updateModel(start, end);
  }
  ...
}
```

**关键**: `renderRows` 是被 xterm.js core 的 RenderService 在检测到 dirty cell 时通过 `requestAnimationFrame` schedule 的。无 dirty 则不调用 → `beginFrame()` 永远不被检查 → vertex buffer 永久保留旧值。

### 5. `_requestClearModel` 永不重置(辅证 — 不是直接根因但揭示上游设计缺陷)

```ts
// TextureAtlas.ts:133-136
private _requestClearModel = false;
public beginFrame(): boolean {
  return this._requestClearModel;     // ← 读但不重置
}

// 整包内只在 133 / 195 / 798 三处赋值,无任何处置 false
```

意味着 atlas 一旦发生 merge,所有 owner renderer 后续每一帧都强制 `_clearModel(true) + _updateModel(0, rows-1)`(过度保守但安全)。但需要 renderer 至少被 schedule 过一次 — 这正是 dormant renderer 漏网的原因。

### 6. resize 为何能修复(`WebglRenderer.handleResize` 第 173-204 行)

```ts
this._model.resize(this._terminal.cols, this._terminal.rows);
for (const l of this._renderLayers) l.resize(this._terminal, this.dimensions);
...
this._rectangleRenderer.value?.handleResize();
this._glyphRenderer.value?.handleResize();   // ← 清空/重建 vertex buffer
this._refreshCharAtlas();
this._clearModel(false);                      // ← 清 model
// 下一帧 renderRows → _updateModel(0, rows-1) 用新 glyph 字段重写
```

故用户描述的"resize 那个终端就恢复,其他不恢复"完全对得上 — handleResize 是唯一能强行清空 per-renderer vertex buffer 的入口。

## mini-term 触发条件复盘

1. 用户在多个 tab / 分屏中同时开 `claude code`(`ai_sessions` 标记的终端);
2. claude TUI 输出大量 box-drawing/ANSI/CJK/emoji 唯一字形 → atlas glyph cache 增长;
3. 任意一个**正在活跃输出**的 claude 终端触发 `_createNewPage` 走 merge 分支;
4. atlas 内 glyph 对象被原地改写;
5. 触发 merge 的终端: 当前帧仍在渲染 → 立刻消费 `beginFrame=true` → 自愈;
6. 其他 claude 终端: 大多数在"等待响应 / 等用户输入",PTY 静默 → render loop dormant → vertex buffer 保留旧 glyph 字段 → 渲染出错位字形;
7. 用户切到那些终端只看到乱码,直到 resize 强制清空 vertex buffer。

## 修复点

`src/utils/terminalCache.ts` 的 `activateWebgl` 内,加 WebglAddon 的 atlas canvas 事件监听,事件触发时遍历整个 cache 调 `term.refresh(0, rows-1)` —— 通过 xterm.js 的 dirty 通道唤醒所有 renderer 进入下一帧,让它们都消费一次 `_requestClearModel` 兜底。

## 参考

- xterm.js issue 4480(WebglRenderer.ts:345 注释提及): atlas page merge 触发模型重置的关联 issue。
- `@xterm/xterm@6.0.0` + `@xterm/addon-webgl@0.19.0`(`package.json` 锁定版本)。

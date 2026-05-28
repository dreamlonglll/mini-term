# 修复多 Claude Code 终端共享 atlas 导致的同时乱码

## 背景

用户报告:同时打开多个跑 `claude code` 的终端(分屏 / 多 tab)一段时间后,所有 AI 终端**同时**出现**形状相同**的乱码 — 中文字符整体被替换成别的字形(如"等**待**" → "等 **UKI**"、"**大概**率" → "**LO51** 率"),同一字符多次出现时乱码完全一致;字符位置 / 行间距 / 底色 / 行号均正常,只有字形像素错位。

工作区缓解路径:
- 对某个乱码终端 resize 整个窗体 → 该终端恢复;
- 其他终端依旧乱码,必须逐个聚焦 + resize 才能恢复。

## 根因(高置信度)

xterm.js `@xterm/addon-webgl@0.19.0` 的 `TextureAtlas` **跨终端共享**:

1. `CharAtlasCache.ts:20` 模块级 `charAtlasCache: ITextureAtlasCacheEntry[] = []`,`acquireTextureAtlas` 按 `(deviceCellWidth, deviceCellHeight, fontFamily, fontSize, colors, devicePixelRatio, deviceMaxTextureSize)` 命中即复用同一个 atlas 实例。mini-term 所有终端 4 项 key 完全一致 → **共享同一个 `TextureAtlas` + 内部 glyph 对象池**。

2. Claude/Codex 是 TUI,输出大量 `(char, bg, fg, ext)` 唯一组合;atlas page 数达到 `maxAtlasPages` 时 `_mergePages` 把 4 个 small page 合并为 1 个 large page,**原地改写**所有相关 glyph 的 `texturePage / texturePosition.x / texturePosition.y / sizeClipSpace.x/y` 字段(`TextureAtlas.ts:207-233`);`_deletePage` 把后续 page 的 `g.texturePage--` 整体左移(`TextureAtlas.ts:235-244`)。

3. `GlyphRenderer` 的 GPU vertex buffer 是 **per-renderer** 的,在过往帧已经写入 `(texturePage, texPosX, texPosY, ...)` 旧值。atlas glyph 字段一旦原地变更,旧 vertex 引用就指向**错误的 page / 错误的 atlas 内偏移** → GPU 仍能 sample 出像素,但是别的字形(因此用户看到的是"换字"而非"色块")。

4. `WebglRenderer.renderRows()` 第 346-348 行有兜底:`beginFrame()`(读 `atlas._requestClearModel`)为真则 `_clearModel(true) + _updateModel(0, rows-1)` 全量重绘。**但 `renderRows` 只在 xterm.js core 检测到 dirty(新 PTY 输出 / 光标移动 / 选区变化)时才被 schedule**。Claude/Codex 在"思考中 / 等待输入"状态长时间无 PTY 输出 → 这些终端的 render loop 完全 dormant → 永远不消费 `_requestClearModel` → 持续乱码。

5. 触发 merge 的终端因正在频繁输出,下一帧立刻消费 `beginFrame()` → 自己恢复;其他 dormant 终端 resize 后通过 `handleResize` 强制 `_glyphRenderer.handleResize()` 清空 vertex buffer + `_clearModel(false)` → 下一帧重绘恢复。

完整证据链与代码引用见 `research/atlas-sharing-root-cause.md`。

## 修复方案

**A. 兜底唤醒**(已选定):

`src/utils/terminalCache.ts` 的 `activateWebgl` 内监听 `WebglAddon.onAddTextureAtlasCanvas` 与 `onRemoveTextureAtlasCanvas`,事件触发时遍历**整个 cache**,对每个已挂载的 terminal 调 `term.refresh(0, rows-1)`。

- `term.refresh()` 把所有行打上 dirty 标记,强迫 xterm.js core 在下一帧 schedule `renderRows`;
- `renderRows` 内 `beginFrame()` 读到 atlas 的 `_requestClearModel === true` → 触发 `_clearModel(true) + _updateModel(0, rows-1)` → 从新 glyph 字段重新生成 vertex buffer → 修复;
- 对触发事件的那个 renderer 多调一次 refresh 无副作用(同帧 dirty 合并)。

为什么选 A 而非"不共享 atlas / 关 WebGL / 上游 patch":
- A 改动 ~15 行,零侵入;
- 不共享 atlas 会让每个终端独自栅格化字形,内存 N 倍 + 首屏抖动;
- 关 WebGL 改回 Canvas 在分屏 + 高频 TUI 输出下卡顿明显;
- 上游 patch 周期不可控,本地不可控。

## 验收

1. **必需 — 类型/编译**: `npx tsc --noEmit` 无错;`cd src-tauri && cargo test` 通过(本次未改 Rust,sanity check)。
2. **手动场景 1 — 多 Claude 并发**: 启动 `npm run tauri dev`,同一项目分屏 4 个终端各跑 `claude code`,持续对话 ~10 分钟,观察是否再现"所有终端同时乱码"。修复前必现,修复后不应再现。
3. **手动场景 2 — 跨 tab 切换**: 打开 3 个项目各跑 `claude`,切到 A 项目让它输出大量内容直到触发 atlas merge,切回 B/C 项目检查终端是否乱码。
4. **回归 — 单终端常规使用**: 单开一个 pwsh 跑 `ls` / `git log` / `cat large.txt`,确保 refresh 监听器不影响普通输出渲染。

## Out of Scope

- 不修复上游 `@xterm/addon-webgl` 的 `_requestClearModel` 永不重置 / dormant renderer 漏唤醒问题(留给 upstream);
- 不调整 `maxAtlasPages` 等 atlas 容量参数;
- 不考虑 WebGL context loss 场景(已有 `onContextLoss → dispose + refresh` 兜底)。

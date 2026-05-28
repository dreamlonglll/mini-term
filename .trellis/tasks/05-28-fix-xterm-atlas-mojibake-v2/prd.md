# fix: v0.4.19 xterm atlas glyph 错位乱码仍复现

## Goal

v0.4.18 通过 9bb05e4 修复了多 Claude Code 终端共享 atlas page merge 后 dormant 终端持续乱码的问题（onAddTextureAtlasCanvas/onRemoveTextureAtlasCanvas 监听 + 广播 `term.refresh`），但用户实际跑 v0.4.19 仍复现：Claude Code 长时间运行后单行/部分行字符错位（中文/数字被替换为合法拉丁字形），**乱码不会自行恢复，必须 resize 或切走再切回来**才会消失。

目标：找到现修复未覆盖的具体路径并补齐，恢复"所有终端最终一致"的不变量。

## What I already know

### 用户提供的现象（screenshot 证据）
- 图片 `clip-1779957312528.png` 显示 Claude Code 进度行：
  - "搭**bi**窗口骨架"（"建"→"bi"）
  - "**ai**nWindow"（"M" 失踪 / 被空字符替换）
  - "1**H**8**H**×192"（"0"→"H"）
- 同帧内紧邻的几行（"5 个 UserControl..."、"可复用控件..."、"dotnet build Release..."）显示正常
- 乱码字符均为「另一个合法字形」—— 典型 atlas glyph 坐标错位
- **不会自行恢复**：resize 单个终端或切走再切回该 tab 可修复
- "运行一段时间后"出现，并非启动即乱

### 已查阅源码（v0.4.19）
- `src/utils/terminalCache.ts:372-395` — 当前修复实现
  - `refreshAllTerminalsForAtlasChange()`: 遍历 cache 调 `term.refresh(0, rows-1)`
  - 在 `loadWebgl` 内 `webgl.onAddTextureAtlasCanvas(refreshAll...)` + `onRemoveTextureAtlasCanvas(refreshAll...)`
- `src/components/TerminalInstance.tsx:94-166` — mount/unmount
  - mount：getOrCreateTerminal → appendChild(wrapper) → rAF: fit + resize + refresh + (rAF: activateWebgl)
  - 已有自己的 IntersectionObserver(container) 在 intersecting 时 fit + refresh
  - unmount：wrapper.remove()（不 dispose）
- `src/components/TerminalArea.tsx:175-190` — 仅渲染 activeTab，非 active tab 的 SplitLayout 不挂载 → 子 TerminalInstance 全部 unmount → 所有 wrapper.remove()
- `node_modules/@xterm/addon-webgl/src/TextureAtlas.ts`
  - `_requestClearModel = true` 后**永不重置为 false**（line 133-135, 195, 798）
  - `_mergePages`（line 207-233）：循环改 glyph.texturePage/texturePosition，每次循环 fire `_onRemoveTextureAtlasCanvas`；`_createNewPage` 在 merge 后 fire `_onAddTextureAtlasCanvas`(mergedPage) + 新建 small page 时再 fire 一次
- `node_modules/@xterm/addon-webgl/src/WebglRenderer.ts:323-360` — renderRows
  - `if (!this._isAttached) { if (screenElement.isConnected && hasSize) { _refreshCharAtlas; _isAttached = true } else return; }`
  - `if (beginFrame()) { _clearModel(true); _updateModel(0, rows-1) }` else partial
  - `_isAttached` 仅 line 268（dimensions <=0 时）会 reset 为 false，**wrapper detach 时不会自动变 false**
- `node_modules/@xterm/addon-webgl/src/GlyphRenderer.ts:359-364` — render
  - 每帧 `if (atlas.pages[i].version !== _atlasTextures[i].version)` 则 `_bindAtlasPageTexture` 上传 canvas 到 per-renderer GPU texture
- `node_modules/@xterm/xterm/src/browser/services/RenderService.ts:148-152` — refreshRows
  - `if (this._isPaused) { _needsFullRefresh = true; return }` —— IntersectionObserver 判定不可见时拦截
  - `_handleIntersectionChange`（line 133-146）：可见性恢复后才 flush 一次 `refreshRows(0, rowCount-1)`

### 已有 spec
- `.trellis/spec/frontend/xterm-webgl-atlas-sharing.md` — v0.4.18 修复后写的约定

## Assumptions (temporary, to validate)

### 主假设 A:`RenderService._isPaused` 拦截 + screenElement detach 时残留
- 切走 tab → `wrapper.remove()` → screenElement 脱离 DOM
- xterm.js core 的 IntersectionObserver(screenElement) fire `intersectionRatio=0` → `_isPaused = true`
- 此期间 atlas merge 发事件 → `refreshAllTerminalsForAtlasChange` 调 `term.refresh` → 被 `_isPaused` 吞掉，仅设 `_needsFullRefresh`
- 切回 tab → wrapper re-attach → IntersectionObserver fire intersecting → `_handleIntersectionChange` 调 `refreshRows(0, rowCount-1)` → **应该**修复
- 但用户报"不会自行恢复"，说明回到 active tab 后 IntersectionObserver 也没正确触发，或者 `_handleIntersectionChange` 的兜底没生效

### 副假设 B:`_isAttached` 状态残留
- `_isAttached` 只在 _refreshCharAtlas 内 dimensions<=0 时才置 false
- detach 后再 attach，`_isAttached` 仍是 true
- renderRows 直接进 beginFrame() 分支 —— `_requestClearModel === true`（永不重置）应该返回 true
- 理论上应该 `_clearModel + _updateModel(0,rows-1)` 修复

### 副假设 C:同 tab 内的可见 terminal 也漏修复
- 用户截图的 terminal 应该当前可见（active tab）
- 但 atlas merge 发生时该 terminal 可能"短暂"不在 viewport 内（被滚到屏幕外、被 modal 覆盖、被 split divider 挤到 0 高度）
- IntersectionObserver fire intersecting=false → `_isPaused=true`
- 后续 terminal 重新进入视口时 IntersectionObserver fire intersecting=true → 兜底 refreshRows(0, rowCount-1)
- 但用户截图时该行已乱 → 兜底**没生效**

### 副假设 D:还有其他 atlas 改写路径漏掉
- 现修复挂的是 `onAddTextureAtlasCanvas` + `onRemoveTextureAtlasCanvas`
- `_drawToCache` 内每次 `activePage.version++`（GlyphRenderer 据此决定是否重传 GPU texture）
- 单纯往现有 page 加 glyph 不触发 onAdd/onRemove，但 vertex buffer 中已存的 glyph 坐标也不变，理论上不会乱
- 但 dormant terminal 的 _atlasTextures GPU 副本会落后 atlas canvas —— **这本身不会乱码**，只是 GPU 端没有最新加入的字形（dormant 用不到）

### 副假设 E:`term.refresh()` schedule rAF 不一定走到 beginFrame() true 分支
- refresh → renderDebouncer → 下一 rAF → _renderRows → renderer.renderRows(start, end)
- renderRows 内：`if (beginFrame()) full update else partial update(start, end)`
- beginFrame() 返回 `_requestClearModel`，一旦 true 永不重置，所以**任何一次走 renderRows 都会 full update** —— 这个应该可靠

## Open Questions (Blocking / Preference only)

- ~~[P-1] 修复方向选择~~ → 已决：**C + E 一起上**（详见 Decision）

## Requirements (evolving)

- [ ] 在 v0.4.19 已有修复基础上补齐遗漏路径
- [ ] atlas glyph 字段改写 / page merge 后，**所有终端**（含可见、不可见、dormant、刚 re-attach）的 vertex buffer 必须在下次 schedule frame 时被重写
- [ ] 修复必须健壮到不需要 resize / 切 tab 用户介入
- [ ] 不引入性能退化（不能每帧都强制 full update / clearTextureAtlas）

## Acceptance Criteria (evolving)

- [ ] 复现脚本：开 4 个 claude code 并发跑 10 分钟以上，不出现"换字"型乱码
- [ ] 复现脚本：切 tab → 跑一段时间 → 切回，原 tab 内容必须正确
- [ ] 切 split pane / 折叠面板后再展开，原 pane 内容必须正确
- [ ] 不依赖 resize / scrollback 切换等用户操作即可自愈
- [ ] 性能：连续输入 5MB 文本耗时与修复前持平（±5%）

## Definition of Done

- 修复 commit + spec 更新 + 任务沉淀
- 至少手工跑过复现路径
- `.trellis/spec/frontend/xterm-webgl-atlas-sharing.md` 补完未覆盖路径 + Wrong/Correct 代码对照
- 不破坏已有 ligatures / WebGL 加载顺序约定

## Out of Scope

- 不动 xterm.js / addon-webgl 上游（锁定 @xterm/xterm@6.0.0 + addon-webgl@0.19.0）
- 不重新引入 per-terminal atlas（内存代价过高，治标不治本，spec 已禁止）
- 不退回 Canvas 渲染（分屏 + 高频 TUI 性能不可接受）

## Feasible Approaches (待用户选择)

### Approach A: 用 `webgl.clearTextureAtlas()` 替代 `term.refresh()` (Recommended)
- **How**: `refreshAllTerminalsForAtlasChange` 内对每个 cache entry 调 `entry.webglAddon?.clearTextureAtlas()`，间接调 atlas.clearTexture() + _clearModel(true) + _requestRedrawViewport
- **Pros**: 
  - clearTextureAtlas 内部走 `this._requestRedrawViewport()` → `onRequestRedraw` 事件 → RenderService.refreshRows(start, end, **isRedrawOnly=true**) → 但仍受 `_isPaused` 拦截，没绕开
  - 实际上 clearTexture 会清空整个 atlas pages，下次 `_drawToCache` 会重新光栅化所有 glyph —— **代价高，可能性能退化**
  - 不一定真能解决 `_isPaused` 拦截
- **Cons**: 性能退化，治标不治本
- **结论**: 实际上不如 Approach B，**降级为非推荐**

### Approach B: 绕开 `_isPaused`，直接 schedule 一次完整 frame
- **How**: 在 atlas 事件 handler 里不用 `term.refresh`，改为：
  1. 对每个 cache entry，invalidate WebglRenderer 的 `_isAttached` 强迫下次 renderRows 走 `_refreshCharAtlas` —— 但 `_isAttached` 是私有
  2. 或者直接读取 `_renderService` 并调 `(renderService as any)._renderRows(0, rows-1)` 强制立刻渲染一次（绕开 debouncer + paused 检查）
  3. 或者用 `term.options.fontSize = term.options.fontSize` 触发 `_handleOptionsChanged` → fullRefresh —— 但这条 hack 链 fragile
- **Pros**: 真正绕开 IntersectionObserver paused 拦截
- **Cons**: 用 `(... as any)` 访问私有 API，xterm.js 升级时易破

### Approach C: 切回 tab 时主动调 `clearTextureAtlas` 一次（兜底）
- **How**: TerminalInstance.tsx 的 mount 路径 / IntersectionObserver intersecting 回调内，主动调 `entry.webglAddon?.clearTextureAtlas()` 强制重传 GPU texture + 重写 vertex buffer
- **Pros**: 不依赖 xterm.js 内部 paused 状态 / 永远在可见时执行一次重置
- **Cons**: 每次切 tab 都付出一次"atlas 清空 + 重画"的代价（轻微闪烁可能）；治标，不阻止"短暂遮挡 → 出现乱码"的中间态

### Approach D: 周期性兜底 refresh + 检测 page 数变化
- **How**: setInterval 每 N 秒检查 atlas.pages.length / pages[i].version，发现变化时广播一次 refresh
- **Pros**: 最简单粗暴
- **Cons**: 不是修根因，仍可能有窗口期乱码；CPU 持续轮询

### Approach E: 探测可能的真实根因（先诊断后下手）
- **How**: 加日志：onAddTextureAtlasCanvas/onRemoveTextureAtlasCanvas 触发时打 console.log + `_isPaused` 状态 + cache size + atlas.pages.length；让用户复现一次再决策
- **Pros**: 不基于假设动手，避免误改
- **Cons**: 需要用户配合复现 + 上传日志

## Decision (ADR-lite)

**Context**: v0.4.19 已有 9bb05e4 修复（atlas 事件监听 + 广播 `term.refresh`），但用户复现"运行一段时间后单行/部分行 atlas glyph 错位乱码，不会自行恢复，必须 resize/切走再回来"。源码分析得出 5 个假设但无法确认哪个是真实根因。Approach A 性能退化，Approach B 依赖私有 API 太脆，Approach D 治标 + CPU 浪费，Approach C 是简单兜底，Approach E 是诊断手段。

**Decision**: 同时上 **Approach C + E**(修订:**事件路径不切 clearTextureAtlas,避免可见闪烁**):
- **C(兜底修复)**:
  - **保留** `refreshAllTerminalsForAtlasChange` 的 `term.refresh` 路径(原 9bb05e4 实现,事件触发时使用,无闪烁)
  - **新增** `clearAtlasForPty(ptyId)` helper:调 `webglAddon.clearTextureAtlas()` 强制清空 atlas + 重置 vertex buffer。会出现 < 1 帧空白,**只能在已知会重绘的时机用**
  - 在 `TerminalInstance.tsx` 的 `visibilityObserver`(IntersectionObserver intersecting 回调)内,在原有 `fit + refresh` 基础上追加调 `clearAtlasForPty(ptyId)` → 切回 tab / 重新可见时强制刷新
  - 在 `getOrCreateTerminal` mount 后的首次 `activateWebgl` 完成后调一次 → mount 时本来就重绘,无额外闪烁感
  - **为什么不切事件路径**: `clearTextureAtlas` 内部把 `vertex buffer + lineLengths` 全 fill(0),GlyphRenderer.render 下一帧画 0 个 cell,可见终端会闪烁。事件触发的可见终端用 `term.refresh` 走 `_clearModel + _updateModel(0, rows-1)` 同帧重写 vertex buffer,无闪烁
- **E(诊断日志)**:
  - 在 `refreshAllTerminalsForAtlasChange` / `clearAtlasForPty` 加 `console.log`:事件来源 / cache size / 每个 term 的 ptyId / `_isPaused` 状态(经反射读) / atlas.pages.length
  - 日志默认 OFF,通过 `localStorage.setItem('miniterm.atlasDebug', '1')` 打开,避免污染普通用户控制台
  - 让用户跑一次复现 → 收集日志 → 后续如果 C 不足再决定要不要上 B

**Consequences**: 
- ✓ C 在事件 handler / mount / 可见性恢复 三个路径冗余刷新,基本消灭所有"vertex buffer 落后于 atlas"窗口期
- ✓ clearTextureAtlas 比 `term.refresh` 更重 —— 但只在事件触发时调用,不是每帧。atlas 清空后下次 `_drawToCache` 重新光栅化,但 glyph 字段都是新坐标 → 不会再错位
- ⚠ 切 tab 回来可能出现 < 1 帧的 "atlas 清空后重画" 闪烁（实测验证）
- ⚠ 如果根因不是 `_isPaused` 拦截,C 也修不了,需要日志辅助才能判断是否要上 B
- E 的日志默认 OFF,不影响生产

## Technical Notes

### 关键源码位置
- 当前修复实现：`src/utils/terminalCache.ts:372-395`
- xterm.js atlas page merge：`node_modules/@xterm/addon-webgl/src/TextureAtlas.ts:150-205`
- `_requestClearModel` 永不重置：`node_modules/@xterm/addon-webgl/src/TextureAtlas.ts:133-135, 195, 798`
- `_isPaused` 拦截：`node_modules/@xterm/xterm/src/browser/services/RenderService.ts:148-152`
- IntersectionObserver 可见性回调：`node_modules/@xterm/xterm/src/browser/services/RenderService.ts:123-146`
- WebglRenderer renderRows：`node_modules/@xterm/addon-webgl/src/WebglRenderer.ts:323-360`
- GlyphRenderer GPU texture 重传时机：`node_modules/@xterm/addon-webgl/src/GlyphRenderer.ts:359-364`

### 关键约束
- 锁定 `@xterm/xterm@6.0.0` + `@xterm/addon-webgl@0.19.0`
- 已有 spec：`.trellis/spec/frontend/xterm-webgl-atlas-sharing.md`
- 已有 spec：`.trellis/spec/frontend/xterm-ligatures-with-webgl-order.md`
- 已修过：9bb05e4 fix（v0.4.18 included）

### Implementation Plan

**PR1（单 PR 上线 C + E）**:

1. `src/utils/terminalCache.ts`:
   - `refreshAllTerminalsForAtlasChange` 改为：每个 entry 优先调 `entry.webglAddon?.clearTextureAtlas()`,失败/无 webglAddon 时回退 `term.refresh(0, rows-1)`
   - 新增 `clearAtlasForVisibilityRestore(ptyId)` helper：可见性恢复时调用,内部走 clearTextureAtlas + refresh
   - 加可开关的诊断日志：`atlasDebugLog(tag, payload)`,读 `localStorage.miniterm.atlasDebug === '1'`
2. `src/components/TerminalInstance.tsx`:
   - mount rAF 后第一次 fit 完成时调 `clearAtlasForVisibilityRestore(ptyId)`
   - `visibilityObserver` 的 intersecting 回调内加 `clearAtlasForVisibilityRestore(ptyId)`
3. 测试与验证：
   - 手工跑 4 个 claude code 并发跑 10 分钟以上,期间多次切 tab / 分屏
   - 开 `localStorage.miniterm.atlasDebug='1'` 跑一次,观察事件触发频次 + `_isPaused` 比例
4. spec 更新：
   - `.trellis/spec/frontend/xterm-webgl-atlas-sharing.md` 补充：
     - 「9bb05e4 修复未覆盖路径」章节：`_isPaused` 拦截 + 切 tab/可见性恢复时的兜底
     - Wrong/Correct 代码对照（v0.4.18 vs v0.4.20）
     - 诊断日志开关使用说明

### Research References (待填)


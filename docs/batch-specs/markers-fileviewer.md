# 批次规格：AI 任务 marker 体系（缺口 #25）+ 文件预览与编辑器（缺口 #29）

> 产出时间：2026-08-19，由规格提取 agent 逐文件对照 Tauri 原版 `src/` / `src-tauri/` 与已迁移的
> `crates/mt-ai`、`crates/mt-project`、`crates/mt-ui`、`crates/mt-app`。
> 对应 `docs/gpui-parity-audit.md:60`（#25 AI 任务 marker 体系）与 `docs/gpui-parity-audit.md:64`（#29 文件预览与编辑器）。
> 连带条目：`docs/gpui-parity-audit.md:22`（快捷键表里「剩 markerPrev/Next 随 marker 批」）、
> `docs/gpui-parity-audit.md:59`（#24 全局搜索「点结果开 FileViewerModal 依赖 #29」）、
> `docs/gpui-parity-audit.md:73`（store 缺失 action 清单里的「markers 系列」）。
>
> **一切以源码实况为准**；每条结论都带源文件与行号，实现时先翻原文再动手。

---

## 0. 摘要

| 节 | 缺口 | 原版组件/文件 | GPUI 现状 | 规模 |
|---|---|---|---|---|
| A | #25 marker 体系 | `useAiSubmitMarker.ts`(41) / `useMarkerHotkeys.ts`(62) / `MarkerList.tsx`(55) / `store.ts` marker 段 / `terminalCache.ts` marker 段 / `PaneGroup.tsx` 按钮+浮层 | 后端**已全通**（`mt-ai` 有 `drain_submits`），前端**零消费** | 中 |
| B | #29 文件预览与编辑器 | `FileViewerModal.tsx`(498) / `CodeEditor.tsx`(350) | 后端**已全通**（`mt_project::fs` 读写齐活），UI **零** | 大 |

两节的**后端都已经迁移完毕**，本批基本纯前端。i18n 词条**两节全部已在** `crates/mt-i18n/src/dict.rs`
（marker 5 条 + fileViewer 15 条 + externalLink 1 条，逐条核对见各节末尾），生成器不必重跑。

---

# A 节：AI 任务 marker 体系（#25）

## A.0 先读这一段：这个功能在原版里**大部分时候不产生任何 marker**

`src/utils/terminalCache.ts:551-557`：

```ts
export function registerAiMarker(ptyId: number): IMarker | null {
  const cached = getCachedTerminal(ptyId);
  if (!cached) return null;
  // 备用缓冲区里打点没有意义,直接跳过:alt buffer 没有 scrollback 可滚,
  // 且 BufferSet.activateNormalBuffer() 退出时会 clearAllMarkers() 全部清掉。
  // 放行 alt screen(见 getOrCreateTerminal)后,走 TUI 的 AI 基本都落在这个分支。
  if (cached.term.buffer.active.type === 'alternate') return null;
```

最后那句注释是作者自己写的：**Claude Code / Codex 这类 Ink/TUI agent 一进来就切备用缓冲区，
打点被整个跳过**。所以装机版里「⚑ N」按钮平时根本不出现，只有在**不切 alt screen 的行式 AI CLI**
（以及 AI 会话被识别但程序仍在主缓冲区的那一小段）才会攒出 marker。

**结论**：本批要做的是一个**低频但语义明确**的功能，不要按「每个 AI 会话都会有一串标记」去做取舍
（尤其不要为了「让它看起来有用」而放开 alt screen 打点——alt grid 的 `max_scroll_limit` 是 0，
见 `alacritty_terminal-0.26.0/src/term/mod.rs:416` 的 `Grid::new(num_lines, num_cols, 0)`，
没有回看缓冲，跳转无处可跳）。

GPUI 侧有一处**天然优于原版**的地方，可以白拿：alacritty 的备用屏是独立 grid（`swap_alt`，
`term/mod.rs:714-734` 只 `mem::swap` 两个 grid），**主屏的 scrollback 在 TUI 期间原封不动**；
而 xterm 从 alt buffer 退回来时 `clearAllMarkers()` 会把主缓冲的 marker 全清掉。
也就是说 GPUI 侧「进 TUI 前打的点，退出 TUI 后仍然有效」。这是改善，不是偏差，做完记一笔。

---

## A.1 marker 何时产生：从 PTY 输入判定到事件

### A.1.1 后端产出（原版 → GPUI 已迁移，**改动为零**）

| 环节 | 原版（Tauri） | GPUI（已在） |
|---|---|---|
| 数据结构 | `src-tauri/src/pty.rs:223-227` `UserSubmit { line, ts }` | `crates/mt-ai/src/tracker.rs:44-49`（同名同字段） |
| 暂存表 | `pty.rs:560` `pending_submits: Arc<Mutex<HashMap<u32, Vec<UserSubmit>>>>` | `tracker.rs:198`（同） |
| 入表点 | `pty.rs:879-893`：`track_input` 处理 `\r`/`\n` 时 | `tracker.rs:446-460`（同） |
| 入表条件 | `pty.rs:880` `!trimmed.is_empty() && self.is_ai_session(pty_id)` | `tracker.rs:447`（同） |
| pane 关闭清表 | `pty.rs:602` `pending_submits.remove(&pty_id)` | `tracker.rs:235` `purge_pane` |
| 取数 | `pty.rs:696-700` `drain_submits(pty_id)`（**取走即清**） | `tracker.rs:297-303` + `perception.rs:107-109` 转发 |
| 送到前端 | `pty.rs:1405-1414` flush 循环里 `app.emit("ai-user-submit", AiUserSubmitPayload { pty_id, line, ts })` | **GPUI 侧没有对应消费者 —— 本批要补的就是这一处** |

判定语义逐条（`tracker.rs:446-460` 与其上方 `track_input` 全文）：

- **只在 AI 会话里打点**：`is_ai_session(pane_id)` 为真才入表。这个身份来自「hook 上报」或「输入检测」
  两条路（见 `CLAUDE.md` 的 AI 会话识别段），marker **不看 `ai-working`/`ai-idle` 状态**，
  只看「这个 pane 现在被认为在跑 AI」。所以 AI 正在跑时用户追加的一句（原版单测
  `tracker.rs:964-973` `track_input_submits_multiple_in_working_window`）同样会打点。
- **空回车不打点**（`tracker.rs:955-961`）。
- **进 AI 的那条命令本身不打点**（`tracker.rs:905-911`）：`claude` 这一行敲下去时
  `is_ai_session` 还是 false，入表判定在「设置会话身份」之前。
- **括号粘贴的多行算一条**（`tracker.rs:984-993`）：粘贴期间的 `\r` 被折成 `\n` 存进行缓冲，
  真正的提交回车才入表，`line` 里带 `\n`。**UI 侧要按这个准备**（见 A.4 截断）。
- **方向键 / Tab 补全不打点**（`tracker.rs:996-1001`）。
- `ts` 是 `SystemTime` 的毫秒（`tracker.rs:452-455`），**UTC epoch ms**，UI 侧按本地时区格式化。

### A.1.2 GPUI 侧新增的消费点：**在 `TerminalPane::write` 里同步取**

原版之所以要经过一次事件（`emit("ai-user-submit")` → `listen` → `requestAnimationFrame` 重试），
是因为 Rust 与 xterm 隔着 IPC。GPUI 侧没有这层隔阂，**取数点应该放在**
`crates/mt-app/src/pane.rs:341-364` 的 `TerminalPane::write`：

```rust
// pane.rs:350-354 现状
self.ai.perception().observe_input_with_line_snapshot(
    self.pty_id, bytes, snapshot.as_deref(),
);
// ↓ 本批在这一句之后紧接着加
for submit in self.ai.perception().drain_submits(self.pty_id) {
    self.push_marker(submit, cx);      // 锚点当场取，见 A.3
}
```

**为什么必须在这里而不是在渲染 tick 里取**：`observe_input` 是同步的，回车那一刻
`pending_submits` 已经有了这条；而此刻 **PTY 还没回显换行**（`pty.write(bytes)` 在
`pane.rs:358-362`，在这段之后），光标仍停在用户输入的那一行上。于是锚点直接取
`term.grid().cursor.point.line` 即可，**不需要原版那个 `registerMarker(-1)` 的减一**
（`terminalCache.ts:558-559` 的 `-1` 正是为了补偿「回显已换行」）。这条时序是本节最省事的地方，
别搬到异步 tick 里去，一搬就得重新面对「减几行」这个问题。

> ⚠️ **上面这段结论后来被推翻了，锚点已改为延后定锚。** 前提「光标仍停在用户输入的
> 那一行上」只对 shell 成立；而**能攒出标记的恰恰不是 shell** —— alt screen 被闸门
> 挡掉之后，剩下的只有 Claude Code 这类 Ink 应用，它走 `log-update`，每帧尾部多一个
> `\n`，等待输入时光标恒定停在渲染块**下方**。实测跳转偏下 1~3 行（偏多少随窗口宽度
> 折行、提示行在不在而变，所以补任何固定值都不成立）。
>
> 现行做法：按 Enter 后武装 `TerminalEmulator::arm_cursor_floor`，取随后 200ms 内
> **光标绝对行的最小值**当锚点 —— Ink 提交那一下会先 `eraseLines` 把光标顶回块首，
> 而 `> 用户输入` 这条 static 消息正好打在块首。不含魔数，且对不做 erase 的行式 CLI
> 自动退化成原来的行为。细节见 `mt_terminal::TerminalEmulator::arm_cursor_floor` 与
> `mt_app::pane::TerminalPane::arm_marks` 的文档注释。

同理，原版 `useAiSubmitMarker.ts:18-33` 那套「拿不到终端就 rAF 重试一次」的兜底**整个不需要**：
GPUI 侧 emulator 与 pane 同生共死，`self.emulator` 永远在。

---

## A.2 数据结构与生命周期

### A.2.1 原版类型（`src/types.ts:605-621`）

```ts
export interface AiUserSubmitPayload { ptyId: number; line: string; ts: number; }
export interface AiMarker {
  id: string;            // UUID,store 索引与 React key
  seq: number;           // 该 pane 内自增序号,UI 显示 "#N"
  ptyId: number;
  line: string;          // 用户输入原文(trim 后)
  ts: number;
  xtermMarkerId: number; // xterm IMarker.id
  inProgress: boolean;   // 最后一个 marker 为 true,新 marker 到来时前一个翻 false
}
```

### A.2.2 store 侧（`src/store.ts`）

| 位置 | 内容 |
|---|---|
| `store.ts:666-671` | 接口：`markersByPty: Map<number, AiMarker[]>` + 4 个 action |
| `store.ts:746` | 初值 `new Map()` |
| `store.ts:1182-1203` | `addMarker`：**先把列表里最后一条的 `inProgress` 翻 false**，再追加新条目（`seq = updated.length + 1`，`inProgress: true`），返回 UUID |
| `store.ts:1205-1211` | `clearMarkersForPty(ptyId)`：整条删 |
| `store.ts:1213-1223` | `pruneDisposed(ptyId, isDisposed)`：过滤掉 xterm 侧已 dispose 的；**过滤后为空则连键一起删** |
| `store.ts:1225` | `getMarkersForPty(ptyId)`：读，缺省空数组 |

**没有条数上限**——`addMarker` 只追加不裁剪。唯一的收缩来自 `pruneDisposed`（xterm 因 scrollback
裁剪而 dispose 掉 marker 时），而 `pruneDisposed` **只在 `addMarker` 之后被调一次**
（`useAiSubmitMarker.ts:22`）。GPUI 侧照抄这个节奏即可：每次新增后跑一遍剪枝。

### A.2.3 清理路径（**五条，一条都不能漏**）

| 场景 | 原版落点 | GPUI 对应落点 |
|---|---|---|
| pane 被关（含关整组） | `store.ts:893-904` + `:925`：`setProjectLayout` 比对新旧 layout 的 ptyId 集合，消失的一并删 | `crates/mt-app/src/store.rs:942-954` `dispose_terminal`（**建议挂这里**，它是「关 pane / 关整组 / 项目移除」三条路的唯一汇合点） |
| 显式回收 pane 运行时 | `src/utils/paneActions.ts:211-219` `disposePane` 里 `clearMarkersForPty` | 同上（GPUI 没有单独的 disposePane） |
| 项目被移除 | `store.ts:813-822` + `:860`：`removeProject` 收集该项目全部 ptyId 后批量删 | `store.rs:354-385` `remove_project` → 内部已走 `dispose_terminal` |
| 远程 pane 重连（清屏重建） | `src/components/PaneGroup.tsx:330` `clearMarkersForPty(oldPtyId)` | 远程项目属 #28，本批不做，记档 |
| xterm 侧实例表 | `terminalCache.ts:548` `disposeTerminal` → `clearMarkerInstances(ptyId)`（`:595-597`） | GPUI 侧不存在「第二份实例表」，见 A.3 |

**GPUI 建议的存放位置**：`AppStore` 加 `markers_by_pty: HashMap<u32, Vec<AiMarker>>`
（`crates/mt-app/src/store.rs:90-128` 的字段区，紧挨 `terminals: HashMap<u32, Entity<TerminalPane>>`）。
**不要放进 `TerminalPane`**——tab 栏的按钮画在 `terminal_area.rs` 里，它拿得到 `store`
但拿 `TerminalPane` 的内部状态要多一次 `read(cx)` 且写起来要 `update`；而且「pane 没了 marker 也没了」
用 `dispose_terminal` 一处就能保证。**审计 `docs/gpui-parity-audit.md:73` 的「store 缺失 action：… markers 系列」
说的就是这个位置。**

---

## A.3 跳转锚点：xterm `IMarker` 的等价物 —— 本节最大的坑

### A.3.1 原版怎么做

`terminalCache.ts:191`：`markerInstancesByPty: Map<number, Map<number, IMarker>>`，
即「pty → (xtermMarkerId → IMarker 实例)」。store 里只存 `xtermMarkerId`（数字），实例活在这张表里。

- 打点：`terminalCache.ts:559` `cached.term.registerMarker(-1)`。xterm 的 `IMarker` **自己会跟着 buffer 移动**
  （新输出把内容顶进 scrollback 时，`marker.line` 自动减小），行号校正是 xterm 白送的。
- 自毁：`terminalCache.ts:567-569` `marker.onDispose(...)` 把它从表里摘掉；xterm 在
  **① scrollback 裁掉那一行 ② 退出 alt buffer** 两种情况下 dispose marker。
- 跳转：`terminalCache.ts:573-579`
  ```ts
  export function scrollToMarker(ptyId, xtermMarkerId) {
    const cached = getCachedTerminal(ptyId);
    const marker = markerInstancesByPty.get(ptyId)?.get(xtermMarkerId);
    if (!cached || !marker || marker.isDisposed) return;
    cached.term.scrollToLine(marker.line);   // ← 把该行滚到视口顶部
    flashLine(cached.term, marker);
  }
  ```
  注意是 **`scrollToLine`（贴视口顶部），不是居中**，而且**无条件滚动**（哪怕这一行已经在视口里）。
  与终端查找的 `scroll_to_current`（`crates/mt-ui/src/terminal/search.rs:628-655`，
  「已在视口里就一动不动，否则滚到视口中间」）**语义不同**，别照抄那一份。
- 命中闪烁：`terminalCache.ts:581-588` `flashLine`：`registerDecoration({ marker, backgroundColor })`，
  底色 `rgba(245, 197, 24, 0.33)`（`terminalCache.ts:193`），`300ms` 后 `dispose`（`:194`）。

### A.3.2 alacritty 侧没有 marker API —— 行号语义与漂移

`crates/mt-ui/src/terminal/search.rs:129-137` 已经把这件事写清楚了：

> `Point::line` 是**grid 绝对行号**（0 = 屏幕第一行，负数 = 回看缓冲），与 display_offset 无关
> —— 但新输出把内容顶进 scrollback 时行号会整体减小，所以命中集合在内容变化后必须重建，不能跨帧长留。

即：**存一个裸 `Line(i32)` 会随输出静默漂移**。查找模块的对策是「每次重搜」；marker 不能重搜
（同一句话可能提交两次，靠文本找不出是哪一条）。

可用的公开量只有三个：`term.grid().display_offset()`、`term.history_size()`、`term.screen_lines()`
（`alacritty_terminal-0.26.0/src/grid/mod.rs:432` / `:516` / Dimensions trait）。
alacritty **不提供**「累计滚出多少行」的计数器，`EventListener` 也没有滚动事件
（`Event` 枚举里只有 Bell/Title/Clipboard/ColorRequest/PtyWrite/TextAreaSizeRequest/ChildExit 等）。

### A.3.3 推荐方案：算术锚点 + 饱和期文本重定位

**主路（算术锚点，零成本、精确）**

打点时记：

```rust
let (line, history) = emulator.with_term(|t| (t.grid().cursor.point.line.0, t.history_size() as i32));
let anchor = line + history;   // 稳定绝对行号
```

取用时：`current_line = anchor - history_size_now`。

推导：内容每被顶上去一行，它的 `Line` 减 1，同时 `history_size` 加 1（`grid/mod.rs:252-276`
`scroll_up` → `increase_scroll_limit(positions)`），两者之和守恒。**在 scrollback 未装满之前这是精确的。**

**饱和是唯一的破绽**：`Config::default().scrolling_history = 10000`
（`alacritty_terminal-0.26.0/src/term/mod.rs:356-366`，`mt-terminal` 用的就是它，
见 `crates/mt-terminal/src/lib.rs:109` `Term::new(Config::default(), …)`）。
`history_size` 涨到 10000 之后 `increase_scroll_limit` 里 `count = min(count, max - history) = 0`
（`grid/mod.rs:176`），此后每次滚动**evict 一行但 `history_size` 不变** → 上式冻结，
marker 会**静默指向错误的行**（比「跳不过去」还糟）。

**补路（饱和后重定位）**：检测到 `history_size() == 10000` 时，锚点转成「文本重定位」：
拿 marker 的 `line` 首个物理行，用查找引擎那套 grid 扫描（`search.rs` 的 `collect_matches` /
逐行取 `grid[Line(l)][Column(c)].c`）从下往上找**最近一条**匹配，按 marker 的 seq 顺序依次向上推进，
找不到 → 该 marker 视为已被裁掉，**从列表里剪掉**（等价于原版 `pruneDisposed` 的语义）。
代价是 O(scrollback × 列)，但只在①已饱和 ②用户真的点了跳转 时才发生，与查找引擎每次重搜同量级。

**最省事的替代**（如果不想写补路）：把两件事一起做——
① 打点时把 `anchor` 与当时的 `history_size` 一起存；
② 每次要用 marker（画列表 / 跳转）前，先判 `history_size() == max_scroll_limit`：
是则**把所有 anchor 早于饱和点的 marker 全剪掉**并在 UI 上不显示。
语义仍然自洽（「太老的标记随 scrollback 一起没了」，与 xterm 的 dispose 同义），
只是会比 xterm 早一点丢弃。**这条要在代码注释里写明是刻意取舍，不是 bug。**

**不要做的事**：不要试图用「每帧采样 `history_size` 差值累加」来重建计数器——饱和后差值恒为 0，
问题原封不动；也不要去读 `Grid` 内部 `Storage::zero`（私有）。

### A.3.4 跳转与闪烁在 GPUI 的落点

- 滚动：`emulator.with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)))`，
  `delta = target_offset - current_offset`，`target_offset = -(current_line)` 使该行落在**视口顶部**
  （`display_offset` 是「往回看多少行」，`row_on_screen = line + display_offset`，
  要 `row_on_screen == 0` 即 `display_offset == -line`）。记得 `clamp(0, history_size)`。
  参照 `search.rs:633-655` 的写法（**只借它的写法，不借它的居中/不动策略**）。
- 闪烁：mt-ui 侧没有 decoration 机制。两条路：
  ① **推荐**：给 `TerminalView` 加一个可选的「高亮行 + 到期时间」字段，画在
  `crates/mt-ui/src/terminal/element.rs` 的 cell 背景之上（与查找高亮同一层，
  `SearchColors` 那套已经证明「命中态进 CellSignature 行签名」能与 damage 缓存共存，
  见 `search.rs` 与 `damage.rs`）。**必须进行签名**，否则 300ms 后撤掉时那一行不会重画。
  ② 简化：跳转后不闪，只滚。原版闪烁是 `300ms` 的一次性提示，砍掉不影响功能，但会丢「跳到哪儿了」的可见反馈——
  用户机器 reduced-motion 为 reduce，但这不是 CSS 动画而是纯色块，仍然可见，建议做。

---

## A.4 终端里的 marker 按钮与浮层

### A.4.1 按钮（`src/components/PaneGroup.tsx:489-503`）

位置：**tab 栏右侧控件簇的最左**，在「终端内查找」按钮之前。

```tsx
{activePane.ptyId !== undefined && markers.length > 0 && (
  <button
    ref={markerBtnRef}
    className="mr-1 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)]
               hover:text-[var(--accent)] hover:bg-[var(--border-subtle)] flex items-center gap-1 transition-colors"
    onClick={() => (markerOpen ? setMarkerOpen(false) : openMarkerPopover())}
    title={t('paneGroup.markerTooltip', { mod: MOD_LABEL })}
    aria-expanded={markerOpen}
  >
    <span>⚑</span>
    <span className="tabular-nums">{markers.length}</span>
  </button>
)}
```

逐条：
- **列表为空就整个不画**（`markers.length > 0`）；这也是 A.0 说的「平时看不见」的直接原因。
- 图标是**文本字符 `⚑`**，不是 SVG。GPUI 侧照抄文本即可（与 `menu.rs` 的 `✓ ` 同一套理由：
  gpui-component 0.5.1 没有 svg 资产）。
- 计数用 `tabular-nums`。gpui 侧没有等宽数字开关，用等宽字体或给个固定宽度即可，量级差异可忽略。
- 点击是 **toggle**（与 Ctrl+F 的「只开不关」不同）。
- tooltip：`paneGroup.markerTooltip` = `"AI 任务标记 ({mod}+Shift+↑/↓ 跳转)"`。

**GPUI 落点**：`crates/mt-app/src/terminal_area.rs:556-615` 那个 `.ml_auto()` 的控件簇，
插在 `split-right` 之前。样式参照同处的 `ctrl(...)` 闭包（`terminal_area.rs:540-549`），
但 marker 按钮是「图标 + 数字」两段，宽度不固定，别复用 `w(px(22.0))` 的方框。

⚠️ **tooltip 的 `{mod}` 插值不能用 `tr!` 宏**：`crates/mt-i18n/src/lib.rs:414-431` 的宏第三分支要求
`$name:ident`，而 `mod` 是 Rust 关键字，`tr!("paneGroup","markerTooltip", mod = "Ctrl")` 编译不过。
直接调 `mt_i18n::t_args("paneGroup", "markerTooltip", &[("mod", mod_label())])`
（现成先例：`crates/mt-app/src/search_modal.rs:320` 与 `:324-326` 的 `mod_label()`）。

### A.4.2 浮层定位与关闭（`PaneGroup.tsx:279-308` + `:593-611`）

```tsx
const [markerAnchor, setMarkerAnchor] = useState<{ top: number; right: number } | null>(null);
const openMarkerPopover = useCallback(() => {
  const rect = markerBtnRef.current?.getBoundingClientRect();
  if (!rect) return;
  setMarkerAnchor({ top: rect.bottom + 4, right: window.innerWidth - rect.right });
  setMarkerOpen(true);
}, []);
```

- **右对齐**到按钮右缘、**下方 4px**。
- 面板：`fixed z-50 rounded-md border shadow-lg`，`background: var(--bg-elevated)`，
  `borderColor: var(--border-subtle)`（`:596-602`）。
- 关闭三条路：① 再点按钮 ② `document` 的 `mousedown` 点在面板与按钮之外（`:294-304`）
  ③ **activePane 的 ptyId 变化**（切 tab / 分屏切换）时无条件关（`:306-308`）。
  **注意原版没有 Esc 关闭**——marker 浮层没进 `overlayStack`。
- 用 `createPortal` 挂 `document.body`。

**GPUI 落点**：照 `crates/mt-app/src/menu.rs` 的层级套路
（`deferred(priority 1)` → `anchored(0,0)` → 全窗透明遮罩 `occlude` + `on_mouse_down` 关闭 →
`anchored(按钮下缘).snap_to_window_with_margin(4px)` → 面板 `occlude`，
见 `menu.rs:26-36` 的图）。**不要直接复用 `menu::show`**：`MenuItem`（`menu.rs:78-127`）只有
label/shortcut/danger/disabled/submenu 五种表达，装不下 marker 行的「#seq + 时间 + 正文 + 进行中圆点」四栏。
建议在 `terminal_area.rs` 里自己写这一层，或把 `menu.rs` 里那套「遮罩 + anchored + snap」抽成
`menu::popover(anchor, element)` 复用（后者更干净，但要动已经稳定的 `menu.rs`，按批次风险自行取舍）。

**是否进 `overlay` 栈**：原版没进。但 GPUI 侧 `crates/mt-app/src/overlay.rs:51-68` 的 kind 表
是「现在压着什么」的唯一真相，且 `menu::show`/终端查找条都登记了。建议**登记一条**
（新增 `kind::MARKER_LIST`），理由与 P 批把查找条并进去一样：不登记的话「浮层开着时按 Ctrl+Shift+F」
会同时开两层且 marker 浮层无人关闭。登记后 Esc 关闭是 GPUI 结构性免费的
（按键沿焦点链派发，见 `docs/gpui-parity-audit.md:71`），比原版多一条关闭路——记档为改善。

### A.4.3 列表内容（`src/components/MarkerList.tsx`）

```tsx
if (markers.length === 0) return <div className="px-3 py-2 text-xs text-[var(--text-muted)]">{t("markerList.empty")}</div>;
return (
  <div className="max-h-80 overflow-y-auto py-1 min-w-[280px]">
    {markers.map((m) => (
      <button key={m.id} className="w-full text-left px-3 py-1.5 text-xs flex items-center gap-2 hover:bg-[var(--bg-hover)]" title={m.line}
        onClick={() => { scrollToMarker(ptyId, m.xtermMarkerId); onClose(); }}>
        <span className="text-[var(--text-muted)] tabular-nums w-8">#{m.seq}</span>
        <span className="text-[var(--text-muted)] tabular-nums w-10">{formatTime(m.ts)}</span>
        <span className="flex-1 truncate">{truncate(m.line)}</span>
        {m.inProgress && <span className="w-1.5 h-1.5 rounded-full shrink-0" style={{ background: 'var(--color-ai-working)' }} aria-label={t("markerList.inProgress")} />}
      </button>
    ))}
  </div>
);
```

| 细节 | 值 | 出处 |
|---|---|---|
| 容器 | `max-h: 20rem`(320px)、竖向滚动、`py-1`、`min-w: 280px` | `MarkerList.tsx:30` |
| 行 | `px-3 py-1.5`、`text-xs`(12px)、`gap-2`、hover `--bg-hover` | `:34` |
| `#seq` 列 | 宽 `w-8`(32px)、`--text-muted`、等宽数字 | `:41` |
| 时间列 | 宽 `w-10`(40px)、`HH:mm`（本地时区，两位补零） | `:11-14`、`:42` |
| 正文 | 占满剩余、单行截断，**先按 40 字截断再交给 CSS truncate**（`truncate(s, 40)` → `s.slice(0,39)+'…'`） | `:16-18`、`:43` |
| `title` 属性 | 完整 `m.line`（悬停看全文；含粘贴多行时的 `\n`） | `:35` |
| 进行中圆点 | 6px 圆、`--color-ai-working` | `:44-50` |
| 空态 | `px-3 py-2`、`text-xs`、`--text-muted`、文案 `markerList.empty` | `:22-28` |
| 点击 | 跳转 **并关闭浮层** | `:36-39` |

⚠️ `--bg-hover` 这个 CSS 变量在 `crates/mt-app/src/ui.rs` 的 `Palette` 里**没有对应项**
（有 `bg_overlay` / `border_subtle`）。全项目其它 hover 用的是 `ui::bg_overlay()`
（如 `file_tree.rs:674`）或 `ui::border_subtle()`（marker 按钮自己用的就是这个）。
统一用 `ui::bg_overlay()`，与文件树行 hover 一致。

⚠️ `inProgress` 的真实语义：`store.ts:1182-1203` 是**唯一**改写它的地方——新 marker 到来时把上一条翻 false。
**没有任何地方在 AI 完成时把最后一条翻 false**。所以「最后一条永远亮着进行中圆点」是原版行为，
照抄即可（想改成跟 `PaneStatus` 联动是**功能变更**，本批不做，记档）。

---

## A.5 markerPrev / markerNext 快捷键

### A.5.1 键位与描述

`src/utils/hotkeys.ts:73-74`：

```ts
{ id: 'markerPrev', combo: { mod: true, shift: true, key: 'ArrowUp'   }, scope: 'global', groupKey: G_MARKER, descKey: 'settings.shortcuts.jumpPrevAi' },
{ id: 'markerNext', combo: { mod: true, shift: true, key: 'ArrowDown' }, scope: 'global', groupKey: G_MARKER, descKey: 'settings.shortcuts.jumpNextAi' },
```

→ GPUI：`KeyBinding::new("ctrl-shift-up", MarkerPrev, Some("Workspace"))` /
`"ctrl-shift-down"`，加进 `crates/mt-app/src/main.rs:944-984` 的 `bindings`，
action 单元结构加进 `main.rs:92-137` 的 `actions!` 块。
键名 `up`/`down` 与 `project_switcher` 那两条一致（`main.rs:966-976`）。

### A.5.2 语义（`src/hooks/useMarkerHotkeys.ts:16-62`）逐条

1. **单独于全局 hotkey 之外**（`useGlobalHotkeys.ts:26-27` 显式把这两个 id 排除），
   理由写在 `useMarkerHotkeys.ts:10-15`：它要维护「这个 pane 上次跳到哪条」的游标。
2. **游标**：`lastJumpRef: Map<ptyId, markerId>`，模块级 ref，**从不清理**（pane 关了条目还在，
   微量泄漏 + 「pty id 复用后游标是旧的」的边界。GPUI 侧把它放进 `AppStore` 与 marker 表同生共死，
   顺手修掉——记为改善）。
3. **目标 pane 解析**（`:32-36`）：先用 DOM 焦点（`focusedPtyIdFromDom()`）且该 ptyId 确实在当前项目布局里；
   否则回退 `resolveActivePane(layout)?.ptyId`。GPUI 对应：`store.focused_pane_id`
   （`crates/mt-app/src/store.rs` 的字段，`terminal_area.rs:346` 有读法）→ 回退 `active_pane_id(project_id)`
   （`store.rs:638`）。
4. **推进规则**（`:41-50`），**非环形**：
   ```
   lastIdx = 有游标 ? indexOf(游标) : -                     // 游标被剪掉时 indexOf === -1
   if (有游标 && lastIdx >= 0)  next = lastIdx + dir
   else                          next = (dir === -1) ? len-1 : 0
   if (next < 0 || next >= len) return                       // 到头就不动，游标也不推进
   ```
   即：**首次按 Ctrl+Shift+↑ 跳到最新一条**（`len-1`），**首次按 ↓ 跳到最早一条**（`0`）；
   之后每按一次移一格；**到两端停住，不绕回**。与终端查找的「环形推进」**相反**，别抄错。
5. **空列表直接返回**（`:39`），不弹任何提示。
6. **没有 `isTypingTarget` / overlay 让路判定**——它是自己挂的 capture 阶段 `window` 监听
   （`:59`），绕过了 `useGlobalHotkeys` 那两道闸。
   **GPUI 侧建议加上 `yields_to_overlay`**（`main.rs:139-158`）：方向键在输入框里有明确语义，
   在设置对话框里按 Ctrl+Shift+↑ 跳终端是意外行为。这是刻意偏差，注释里写明。
7. 跳转动作与浮层点击**完全一致**：`scrollToMarker(ptyId, target.xtermMarkerId)`（`:56`），
   即滚到视口顶部 + 闪 300ms，**不关任何东西**（浮层没开的时候按也照跳）。

---

## A.6 i18n 与样式对账

| key | 中文 | GPUI dict | 消费点 |
|---|---|---|---|
| `markerList.empty` | 暂无标记 | ✅ `crates/mt-i18n/src/dict.rs:426-430` | 列表空态 |
| `markerList.inProgress` | 正在进行 | ✅ 同上 | 圆点的 aria-label（GPUI 无 aria，可用 tooltip 或直接不用） |
| `paneGroup.markerTooltip` | `AI 任务标记 ({mod}+Shift+↑/↓ 跳转)` | ✅ `dict.rs:540`(zh) / `:569`(en) | 按钮 tooltip，**插值见 A.4.1 的关键字坑** |
| `settings.shortcuts.aiTaskMarks` | AI 任务标记 | ✅ `dict.rs:1046` / `:1233` | 快捷键设置页分组名，**GPUI 尚无该页面**（属设置页批），本批不消费 |
| `settings.shortcuts.jumpPrevAi` | 跳转到上一个 AI 任务提交 | ✅ `dict.rs:1058` / `:1245` | 同上 |
| `settings.shortcuts.jumpNextAi` | 跳转到下一个 AI 任务提交 | ✅ `dict.rs:1057` / `:1244` | 同上 |

**本节 i18n 缺口：零。** 不需要动 `src/i18n/locales/*.ts`，不需要重跑 `gen_from_ts.mjs`。

样式变量对照（`src/styles.css` → `crates/mt-app/src/ui.rs`）：

| CSS 变量 | ui.rs |
|---|---|
| `--text-muted` | `ui::text_muted()` (`ui.rs:270`) |
| `--accent` | `ui::accent()` (`:276`) |
| `--border-subtle` | `ui::border_subtle()` (`:284`) |
| `--bg-elevated` | `ui::bg_elevated()` (`:248`) |
| `--color-ai-working` | `ui::color_ai_working()` (`:310`) |
| `--bg-hover`（marker 行 hover） | **无对应**，用 `ui::bg_overlay()` (`:252`) |
| `--radius-sm` | 现有代码统一写 `px(3.0)`（如 `terminal_area.rs:546`） |

---

## A.7 A 节实现清单（12 条）

1. `AppStore` 加 `markers_by_pty: HashMap<u32, Vec<AiMarker>>` + `marker_cursor: HashMap<u32, String>`，
   以及 `add_marker` / `clear_markers_for_pty` / `prune_markers` / `markers_for_pty` 四个方法（照 `store.ts:1182-1225`）。
2. `AiMarker` 结构（`id`/`seq`/`pty_id`/`line`/`ts`/`anchor`/`in_progress`）；`anchor` 取代 `xtermMarkerId`。
3. `TerminalPane::write` 里 `observe_input_*` 之后 `drain_submits` → 取锚点 → 回调 store（A.1.2）。
4. **alt screen 打点闸门**：`emulator.mode().contains(TermMode::ALT_SCREEN)` 为真直接跳过（A.0）。
5. 锚点求值 + 饱和期处理（A.3.3），带单测：未饱和时输出 N 行后 `current_line` 精确减 N。
6. `dispose_terminal` 里清 markers 与游标（A.2.3）。
7. tab 栏 `⚑ N` 按钮（A.4.1），含 `t_args` 插值。
8. marker 浮层（A.4.2 + A.4.3），登记 `kind::MARKER_LIST`。
9. `scroll_to_marker`：滚到**视口顶部**（A.3.4），带单测。
10. 命中行闪烁 300ms（可选但建议，A.3.4）。
11. `MarkerPrev` / `MarkerNext` action + 键位 + 非环形推进逻辑（A.5），推进规则带单测（首次 ↑ 到末尾、首次 ↓ 到开头、到头不动）。
12. 记档：alt screen 下不打点导致「按钮平时不出现」是**原版行为**；GPUI 侧 marker 活过 TUI excursion 是改善。

---

# B 节：文件预览与编辑器（#29）

## B.0 GPUI 现状：**预览器完全不存在**，两个入口都退到了「外部编辑器」

| 位置 | 现状 | 出处 |
|---|---|---|
| 文件树点文件 | **单击无反应，双击调外部编辑器** | `crates/mt-app/src/file_tree.rs:675-681` + `:190-195` `open_file` |
| 全局搜索点结果 | **单击就调外部编辑器**（原版单击是预览、双击才是编辑器） | `crates/mt-app/src/search_modal.rs:253-265`，模块注释 `:23-24` 已记为偏差 |
| 右键「使用默认工具打开」 | 已在 | `file_tree.rs:363-375` |

后端**已全部就绪**，本批不需要动 `mt-project`：

| 前端 invoke | Rust 函数 | 位置 |
|---|---|---|
| `read_file_content` | `mt_project::fs::read_file_content(project_root, path) -> FileContentResult` | `crates/mt-project/src/fs.rs:322-347` |
| `write_file_content` | `mt_project::fs::write_file_content(project_root, path, content)` | `crates/mt-project/src/fs.rs:350-361`（内部 `atomic_write`） |
| `open_path_with_default_app` | `mt_project::editor::open_path_with_default_app(path)` | `crates/mt-project/src/editor.rs:66` |
| `open_in_editor` | `mt_project::editor::open_in_editor(editor, path)` | `crates/mt-project/src/editor.rs:41` |
| `fs-change` 事件 | `mt_project::watch::FsWatcher` + 注入式 sink | `crates/mt-project/src/watch.rs:33-48`，用例见 `file_tree.rs:41-60` |

---

## B.1 打开入口全表

| # | 入口 | 原版行为 | 出处 | GPUI 落点 |
|---|---|---|---|---|
| 1 | **文件树单击文件行** | 开 `FileViewerModal`，无 `highlightLine` | `src/components/FileTree.tsx:151-155`（`handleToggle`：`!entry.isDir → onViewFile(path)`）+ `:196` | `file_tree.rs:675-681` 的 `on_click`：**单击**改为开预览器，双击保留外部编辑器 |
| 2 | **文件树键盘 Enter / Space** | 同上（走同一个 `handleToggle`） | `FileTree.tsx:197-209` | 文件树目前无键盘导航，按现状跳过并记档 |
| 3 | **全局搜索单击结果**（文件名 & 内容两种） | 开 `FileViewerModal`，`highlightLine = item.lineNumber` | `src/components/SearchModal.tsx:203-211` + `:316`/`:85` | `search_modal.rs:253-265` `open_result` 改为开预览器；把 `item.line_number` 传下去 |
| 4 | 全局搜索**双击**结果 | `invoke('open_in_editor')` | `SearchModal.tsx:213-222` | 现状已是单击调编辑器，改成「单击预览 / 双击编辑器」即与原版一致 |
| 5 | Markdown 预览里的**本地文件链接** | 在同一个弹窗内跳转，压历史栈 | `FileViewerModal.tsx:169-176` / `:210-213` | 见 B.6 |
| 6 | 右键「使用默认工具打开」 | 不经预览器，系统关联程序 | `FileTree.tsx:243-249` | 已在，不动 |

**远程项目守卫**：`FileTree.tsx:680-683` 的 `handleViewFile` 首行 `if (isRemote) return;`
——远程项目 MVP 只读浏览，不进预览器。GPUI 侧远程项目属 #28，暂无，记档。

**弹窗宿主关系**：原版 `FileViewerModal` 是**两处各挂一份**（`FileTree.tsx:838-848` 与
`SearchModal.tsx:356-363`），都用 `lazy()` 懒加载（`FileTree.tsx:23-24`、`SearchModal.tsx:13-14`，
理由「CodeMirror + react-markdown 数百 KB」）。GPUI 没有代码分割这回事，
**建议做成单例**：`crates/mt-app/src/file_viewer.rs` 新模块 + `prompt::open_guarded(kind::FILE_VIEWER, …)`，
两个入口都调同一个 `file_viewer::open(store, project_root, path, highlight_line, window, cx)`。
防叠开、Esc 关闭、快捷键让路一次到位（`overlay.rs:51-68` 加一条 kind）。

---

## B.2 文件类型判定与内容读取

### B.2.1 前端三条正则（`FileViewerModal.tsx:27-37`）

```ts
isMarkdownFile: /\.(md|markdown|mkd|mdx)$/i
isImageFile:    /\.(png|jpe?g|gif|bmp|webp|svg|ico|avif|tiff?)$/i
isHtmlFile:     /\.html?$/i
```

### B.2.2 后端判定（`crates/mt-project/src/fs.rs:311-347`）

```rust
pub const MAX_FILE_VIEW_SIZE: u64 = 1_048_576; // 1MB，读写共用（fs.rs:319-320）

read_file_content:
  verify_under_project_root(...)        // 越界拒绝
  !p.is_file() -> bail
  len > 1MB    -> { content: "", is_binary: false, too_large: true }
  String::from_utf8(bytes):
      Ok  -> { content, false, false }
      Err -> { content: "", is_binary: true, false }   // 非 UTF-8 == 二进制
```

**编码只认 UTF-8**：任何非 UTF-8 文件（GBK 的中文源码、UTF-16 的 Windows 文本）
一律判成「二进制，不支持预览」。这是原版行为，**照抄，不要顺手加编码探测**——
写回侧 `write_file_content` 只接受 `&str`，加了读侧探测就必须同步写侧编码，是另一个批次的活。

### B.2.3 四种渲染分支（`FileViewerModal.tsx:409-495`）

| 条件 | 画什么 | 出处 |
|---|---|---|
| `loading` | 居中 `fileViewer.loading` | `:410-414` |
| `error`（invoke 抛错） | 居中红字，直接显示错误原文 | `:415-419` |
| `isImg`（**扩展名判定，不读文件**） | `<img src={convertFileSrc(path)} class="max-w-full max-h-full object-contain">`，容器 `p-6` 居中 | `:420-429` |
| `result.isBinary` | 居中「二进制文件，不支持预览」+ 「使用默认工具打开」按钮 | `:430-440` |
| `result.tooLarge` | 居中「文件过大（>1MB）」+ 同一个按钮 | `:441-451` |
| `canEdit` | 编辑器 / Markdown 预览 / HTML 预览三选一，见 B.3 | `:452-494` |

`canEdit = !!result && !result.isBinary && !result.tooLarge && !isImg`（`:244`）。

⚠️ **图片分支的 GPUI 坑**：`isImageFile` **包含 `svg`**，而 gpui 0.2.2 的
`img(Image::from_bytes(ImageFormat::Svg, …))` 漏了 RGBA→BGRA 交换 → **红蓝互换**，
且 tiny-skia 的预乘 alpha 让抗锯齿边缘也对不上（原文档：`crates/mt-ui/src/icons/vector.rs:11-13`）。
位图走 `gpui::img(Resource::Path)` 没问题（`crates/mt-ui/src/background.rs` 的 `ImageAssetLoader`
就是直接读盘的先例）。**svg 要么单独退到「使用默认工具打开」，要么等上游修**——两条都可以，
但**必须显式处理**，不能让用户看到红蓝互换的图还以为是自己文件坏了。
`ico` / `avif` / `tiff` 是否被 `image` crate 支持也要单测一遍（Cargo.lock 里 `image 0.25.10`
带 `gif`/`png`/`tiff`/`ravif`/`image-webp`/`zune-jpeg`，**ico 需要 `ico` feature**，默认可能没开）。

---

## B.3 预览态与编辑态

### B.3.1 切换控件（`FileViewerModal.tsx:355-374`）

**只有 Markdown 与 HTML 有「预览 / 源码」段控件**（`(isMd || isHtml) && canEdit`）。
其余文件类型直接就是编辑器，无切换。

```tsx
<div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs">
  <button className={preview ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}
    onClick={() => { setPreviewDraft(draftRef.current !== savedRef.current ? draftRef.current : null); setPreview(true); }}>
    {t("fileViewer.preview")}
  </button>
  <button className={!preview ? '…accent…' : '…muted…'} onClick={() => setPreview(false)}>{t("fileViewer.source")}</button>
</div>
```

两条要害：

1. **切到预览时拍一份草稿快照**（`previewDraft`，`:117-118` 的注释）：预览渲染的是「正在编辑的内容」，
   不是磁盘旧文；干净时置 `null`，预览直接用 `diskContent`。
2. **编辑器在预览态只隐藏不卸载**（`:481-482` `className={(isMd||isHtml) && preview ? 'hidden' : 'h-full'}`）——
   保住未保存的草稿与撤销栈。GPUI 侧对应：`InputState` 的 `Entity` **一直活着**，
   render 时不画它（`.when(!preview, |el| el.child(input))`），**不要每次切换重建 `InputState`**。

### B.3.2 三个内容源变量的关系（`FileViewerModal.tsx:109-128`）

```
savedRef.current  = 磁盘上最后一次已知内容（载入 / 保存成功时更新）
draftRef.current  = 编辑器当前全文（每次 onDocChange 更新）
diskContent       = 磁盘现内容的 state 投影（预览渲染用它，不用 result.content）
dirty             = draftRef !== savedRef 的 UI 投影（真值以两个 ref 比较为准）
```

`:119-122` 的注释解释了为什么预览不能直接用 `result.content`：那是「打开时」的内容，保存后就旧了；
也不能改 `result`，那会换掉编辑器的 `value` 触发重建、丢撤销栈。
GPUI 侧同理：`InputState` 的 `set_value` 会重置 undo，**只在「换文件 / 显式重载」时调**。

---

## B.4 语法高亮：原版方案与 GPUI 候选

### B.4.1 原版（`src/components/CodeEditor.tsx`）

- **CodeMirror 6**，扩展清单在 `:251-293`：行号、活动行、折叠槽、历史（撤销）、括号匹配、
  自动补括号、矩形选择、选中同词高亮、`search({top:true})`（Ctrl+F 面板）、缩进、拖放光标。
- **语言按需加载**：`@codemirror/language-data` 的 `LanguageDescription.matchFilename(languages, fileName)`
  → 动态 `import()` 语言包（`:300-308`）；匹配不到就是纯文本。
- **主题不写死颜色**：`HighlightStyle.define` 的每条 `color` 都指向 CSS 变量
  `--syn-keyword` / `--syn-string` / `--syn-number` / `--syn-function` / `--syn-type` /
  `--syn-property` / `--syn-tag` / `--syn-comment` / `--syn-operator`（`CodeEditor.tsx:75-103`），
  变量定义在 `src/styles.css:50-59`，各自引用应用现有色板：
  ```
  --syn-keyword: var(--color-ai);        --syn-string:   var(--color-success);
  --syn-number:  var(--color-warning);   --syn-function: var(--color-file);
  --syn-type:    var(--color-folder);    --syn-property: var(--color-info);
  --syn-tag:     var(--color-error);     --syn-comment:  var(--text-muted);
  --syn-operator: var(--text-secondary);
  ```
  **切主题不重建编辑器**。
- 编辑器 chrome 主题：`CodeEditor.tsx:106-179`，全部走应用色板。
- **CodeMirror 内置 UI 的中文文案**自带一张 21 条的表（`:182-201`），
  按 `useI18nStore().lang === 'zh'` 注入 `EditorState.phrases`（`:269`）。
- 折行策略：`shouldWrap(fileName)` —— **只有 md/markdown/mkd/mdx/txt 折行，代码不折**（`:203-206`、`:288`）。
- **CRLF 往返**（`:242-252`）：`value.includes('\r\n')` 时设 `EditorState.lineSeparator.of('\r\n')`，
  否则 CM 的 `doc.toString()` 一律用 `\n` 拼接 → Windows 上改一个字就是整文件行尾变更的 diff。
  **这条在 GPUI 侧必须原样保住**，见 B.9 的坑。

### B.4.2 GPUI 候选评估

**结论：用 `gpui_component::input` 的 CodeEditor 模式，开 `tree-sitter-languages` feature。**

| 候选 | 评估 |
|---|---|
| **gpui-component `InputState::code_editor(lang)`** ✅ | `~/.cargo/registry/…/gpui-component-0.5.1/src/input/state.rs:452-456`。默认带：语法高亮 / 自动缩进 / 行号 / 缩进参考线 / 5 万行大文本 / **`searchable` 自动置真**（`:454`，即 Ctrl+F 面板，`src/input/search.rs`）。`line_number(bool)` (`:473`)、`soft_wrap(bool)` (`:705`)、`rows(n)` (`:492`) 齐活。**已经是工作区依赖，零新增 crate。** |
| 语言覆盖 | `src/highlighter/languages.rs:5-46`：**不开 feature 只有 `Json`**；开 `tree-sitter-languages` 得 30 种（bash/c/cmake/csharp/cpp/css/diff/ejs/elixir/erb/go/graphql/html/java/javascript/jsdoc/make/markdown/proto/python/ruby/rust/scala/sql/swift/toml/tsx/typescript/yaml/zig/plain）。开法：根 `Cargo.toml:39` 改成 `gpui-component = { version = "0.5.1", features = ["tree-sitter-languages"] }`。代价是 30 个 tree-sitter 语言 crate 的**编译时间**（都带 `cc` 构建脚本），首次 build 明显变慢——这是本节唯一的实质成本，值得。 |
| 配色 | `src/highlighter/registry.rs:436-460` `HighlightTheme` + `HighlightThemeStyle`，从 `cx.theme().highlight_theme` 取。要把它映射到 `crates/mt-app/src/ui.rs` 的 `Palette`（照 `--syn-*` 九条），落点建议 `crates/mt-ui/src/theme_bridge.rs`（那儿已经在做 gpui-component `ThemeConfig` 的映射）。 |
| 自定义语言 | `LanguageRegistry::register(lang, &LanguageConfig)`（`registry.rs:480`）——原版 CM 覆盖的语言更多（`@codemirror/language-data` 上百种），差的那些落到 `Plain`，可接受。 |
| **syntect** ❌ | Cargo.lock 里**没有**；引入等于新增 onig/fancy-regex + 语法包资产，且与 gpui-component 的高亮体系并存两套配色。不选。 |
| **裸 tree-sitter** ❌ | Cargo.lock 有 `tree-sitter 0.25.10` + `tree-sitter-json 0.24.8`（`Cargo.lock:6909` / `:6923`），但都是 gpui-component 的传递依赖。自己接等于重写 gpui-component 已有的那一层。不选。 |
| Markdown 预览 | **`gpui_component::text::TextView::markdown(id, md, window, cx)`**（`src/text/text_view.rs:402-434`），底层 `markdown 1.0.0`（`Cargo.lock:3603`）。已在依赖里，零新增。GFM 表格/任务列表的支持度要现场验一遍。 |
| HTML 预览 | `TextView::html(...)`（`text_view.rs:438`），底层 `html5ever` + `markup5ever_rcdom`。**它是「把 HTML 当富文本渲染」，不是浏览器**——原版用的是 `<iframe srcDoc sandbox>`（`FileViewerModal.tsx:454-460`），语义差得远（无 CSS、无 JS、无相对资源）。见 B.6 的取舍。 |

**只读场景**：gpui-component 的 `InputState` 没有 read-only，只有 `Input::disabled(true)`
（`src/input/input.rs:131-135`），而 disabled 会换掉背景色（`:258`）。原版 `CodeEditor` 有
`readOnly` prop（`CodeEditor.tsx:62`、`:289` `EditorState.readOnly.of(true)`）但
**FileViewerModal 从不传它**（`:483-491` 的调用点没有 `readOnly`）——所以原版里没有只读场景，
本批也不需要。`canEdit=false` 的三种情况（图片/二进制/过大）压根不画编辑器。

---

## B.5 保存流程

### B.5.1 保存动作（`FileViewerModal.tsx:251-272`）

```
if (savingRef.current || draftRef.current === savedRef.current) return;   // 干净或在保存中：静默返回
text = draftRef.current; savingRef = true; setSaving(true); setSaveError('')
await invoke('write_file_content', { projectRoot, path: currentPath, content: text })
  成功 → savedRef = text; lastSaveAtRef = Date.now(); setDiskContent(text);
         setDirty(draftRef.current !== text)      // ← 保存期间又敲了字：按最新草稿重新比对，不是直接置 false
         setExtChanged(false)
  失败 → setSaveError(String(e))                  // 顶部挂红条，不弹窗
finally → savingRef = false; setSaving(false)
```

「干净时 Ctrl+S 静默返回」的注释（`:252`）：**Ctrl+S 是肌肉记忆，不该弹任何东西**。

### B.5.2 脏标记 UI

- 标题旁一颗 6px 圆点，`bg-[var(--accent)]`，`title = fileViewer.unsaved`（`:330-335`）。
- 保存按钮：脏时实心 accent、干净时描边灰且 `cursor-default`；`disabled={!dirty || saving}`；
  文案在 `fileViewer.save` / `fileViewer.saving` 之间切；`title="Ctrl+S"` 硬编码（`:341-354`）。
- **只有 `canEdit` 时才画保存按钮**。

### B.5.3 外部修改冲突（`FileViewerModal.tsx:274-283`）

```ts
useTauriEvent<FsChangePayload>('fs-change', (payload) => {
  if (!open || isImg || !result) return;
  const norm = (s) => s.replace(/\\/g,'/').toLowerCase();
  if (norm(payload.path) !== norm(currentPath)) return;
  if (Date.now() - lastSaveAtRef.current < 2000) return;   // 自己落盘的回声，不算「外部」
  if (draftRef.current !== savedRef.current) setExtChanged(true);   // 脏：挂提示条让用户决定
  else setReloadNonce(n => n + 1);                                  // 干净：静默重载跟上磁盘
});
```

提示条（`:393-406`）：`--color-warning` 文字 + `--accent-subtle` 底，文案 `fileViewer.externallyChanged`，
右侧一个下划线按钮 `fileViewer.reloadDiscard`（点了清 `extChanged` + `reloadNonce++`）。
保存失败条（`:385-392`）：`--color-error` 文字 + `--color-error-muted` 底，
`{t('fileViewer.saveFailed')}: {saveError}`，单行截断 + `title` 全文。

**路径比对是「反斜杠归一 + 小写」**——Windows 上 notify 回来的路径大小写可能与用户点的不一致。
GPUI 侧同理，别直接 `PathBuf == PathBuf`。

**GPUI 接线**：`mt_project::watch::FsWatcher`（`crates/mt-project/src/watch.rs:33-48`）已有注入式 sink，
`file_tree.rs:41-60` 是现成用例（sink 里往 channel 丢，主线程前台任务醒来后 `update`）。
预览器要 `watch` 的是**当前文件的父目录**（notify 是目录级监听），
关闭弹窗时 `unwatch`——`FsWatcher` 内部有引用计数（`watch.rs:9` 的注释），与文件树同时监听同一目录是安全的。

### B.5.4 「2 秒回声窗口」的已知边界

`lastSaveAtRef` 的 2000ms 窗口会**顺带吞掉这 2 秒内真正的外部修改**（比如保存后立刻被
formatter/pre-commit 改写）。这是原版行为，照抄并在注释里记一笔即可，不要改成内容比对
（那会引入「保存内容恰好等于外部改写结果」的另一类误判）。

---

## B.6 Markdown / HTML 预览与链接跳转

### B.6.1 Markdown 渲染（`FileViewerModal.tsx:462-480`）

`ReactMarkdown` + `remarkGfm`（表格/删除线/任务列表）+ `rehypeRaw`（**允许内嵌原始 HTML**），
容器 `.md-preview p-6 max-w-[860px] mx-auto`。三个自定义组件：

1. **标题加锚点 id**（`:81-93`）：GitHub 风格 slug —— `trim().toLowerCase()`，
   去掉 `[^\w一-龥\s-]`，空格转 `-`（`:72-78`），使 `[文字](#标题)` 可滚动定位。
2. **`img` 相对路径解析**（`:145-150` `resolveImgSrc`）：非 `http/data/blob` 的 src
   拼成 `fileDir + '/' + src` 后 `convertFileSrc`。
3. **`a` 点击拦截**（`:187-213` `handleLinkClick`），**首行必须 `preventDefault`**，否则 WebView 整个重载：
   - `^https?://` → `openExternalUrl(href)`：**先弹确认框**（`src/utils/externalLink.ts:11-15`，
     文案 `externalLink.openConfirm`），确认后系统浏览器打开；
   - `#锚点` → `contentRef.querySelector('[id=…]')` + `scrollIntoView({behavior:'smooth'})`；
   - 其它协议（`mailto:`/`tel:`，**排除 Windows 盘符 `X:\`**，正则 `:206`）→ 交给系统；
   - 否则当本地文件 → `resolveLocalHref` 规范化（`:40-58`：去 `#`/`?`、`decodeURI`、
     反斜杠转正斜杠、逐段消 `.`/`..`、区分 Windows 绝对/POSIX 绝对）→ `navigateTo(target)`。

### B.6.2 弹窗内跳转与返回栈（`:103-105`、`:169-185`、`:238-242`、`:300-303`）

- `currentPath` + `history: string[]`。`navigateTo`：先过 `confirmDiscard`（B.7），再压栈换路径。
  **用两次独立 `setState`，不在 updater 里嵌套**（`:167-168` 的注释：StrictMode 下 updater 会被二次调用导致重复入栈）。
- `goBack`：同样先 `confirmDiscard`，弹栈。
- 返回箭头 `←` 只在 `history.length > 0` 时画（`:317-325`），`title = fileViewer.back`。
- 外部传入的 `filePath` 变化或重新打开时 **重置 `currentPath` 并清空 history**（`:239-242`）。
- 跳转后内容区 `scrollTop = 0`（`:301-303`）。
- `highlightLine` 只在 `currentPath === filePath` 时才传给编辑器（`:486`）——跳走之后行号就失效了。

### B.6.3 HTML 预览（`:134-143`、`:454-460`）

`<iframe srcDoc={htmlSrcDoc} sandbox="allow-same-origin" className="w-full h-full border-0 bg-white">`，
`htmlSrcDoc` 把所有 `src|href|poster="相对路径"` 重写成 `convertFileSrc(fileDir + '/' + url)`
（排除 `https?:`/`data:`/`blob:`/`mailto:`/`tel:`/`#`/`javascript:`）。

**GPUI 侧没有 iframe**。三条路，建议选 ①：

1. **HTML 直接不做预览态**：`.html` 文件只有源码编辑器（把 `(isMd || isHtml)` 缩成 `isMd`）。
   记档为已知缺口。理由：`TextView::html` 是富文本渲染器，画出来的东西与浏览器差异大到会误导人
   （无 CSS、无 JS、无相对资源），比「不提供」更糟。
2. `TextView::html` 凑合渲染 —— 只在明确接受「这不是浏览器预览」时选。
3. `gpui-component` 的 `webview` feature（依赖 `wry`）—— **不选**：为一个边角功能引入整套 WebView2 依赖，
   与「GPUI 改造是为了去掉 WebView」的整体方向直接冲突。

### B.6.4 `.md-preview` 样式表（`src/styles.css:813-943`，131 行）

要逐条移植到 gpui 的 `TextView` style 上（`gpui_component::text::style` 模块）。要点：
容器 `font-size: 1.08rem / line-height: 1.7`；h1 1.8em + 下边框、h2 1.4em + 下边框、h3 1.15em、h4 1em，
`margin-top 1.4em / bottom 0.6em`；`p` 上下 0.8em；`a` 用 `--accent` 无下划线、hover 才有；
行内 `code` 用 `--bg-elevated` 底 + `--accent` 字 + `--app-font-mono` + 0.88em；
`pre` 用 `--bg-elevated` + `--border-subtle` 边 + `--radius-md` + `14px 16px` 内边距；
`blockquote` 左 3px `--accent` 竖线 + `--accent-subtle` 底；表格 `border-collapse` + `--border-default` 边 +
表头 `--bg-elevated` + 偶数行 `--bg-surface`；`img` `max-width:100%` + `--radius-md`；
`del` 用 `--text-muted`；任务列表 checkbox `accent-color: var(--accent)`。

**代码块高亮**：原版 `.md-preview pre code` **不做语法高亮**（`styles.css:874-880` 只设了颜色与字号）。
gpui-component 的 `TextView` 会给代码块上高亮（它持有 `highlight_theme`）——这是**改善**，
但要确保配色走的是同一份 `--syn-*` 映射，否则 Markdown 里的代码块与编辑器里的同一段代码颜色不一样。

---

## B.7 快捷键与关闭语义

| 键 | 行为 | 出处 |
|---|---|---|
| **Ctrl/Cmd+S** | 全局 capture 监听（`window.addEventListener('keydown', h, true)`），`!shift && !alt`，`preventDefault + stopPropagation`，调 `handleSave` | `FileViewerModal.tsx:285-298` |
| **Esc / 点遮罩 / ✕** | 都走 `requestClose`（`:158-164`） | `:312`、`:375-380` |
| **Ctrl+F**（编辑器内） | CodeMirror 的搜索面板（`search({top:true})` + `searchKeymap`） | `CodeEditor.tsx:268`、`:280` |
| Tab | `indentWithTab` | `CodeEditor.tsx:283` |
| Ctrl+Z / Ctrl+Y | `history()` + `historyKeymap` | `CodeEditor.tsx:256`、`:281` |

**两段式退出**（`FileViewerModal.tsx:158-164`）：

```ts
const requestClose = useCallback(() => {
  if (editorApiRef.current?.closeSearchIfOpen()) return;   // ① 编辑器搜索面板开着：只关面板
  void confirmDiscard().then(ok => { if (ok) onClose(); });  // ② 再问「有未保存修改吗」
}, [confirmDiscard, onClose]);
```

`closeSearchIfOpen` 的实现与「为什么不用全局监听」写在 `CodeEditor.tsx:310-321`：
编辑器是数据到达后才挂载的，`window` capture 注册必然晚于 Modal，抢不到事件，所以走 `apiRef` 句柄。

`confirmDiscard`（`:153-156`）：`draftRef === savedRef` 直接返回 true；否则
`showConfirm(t('fileViewer.unsavedTitle'), t('fileViewer.unsavedMessage'))`。
**三处调用**：关闭（`:160`）、markdown 链接跳转（`:171`）、返回（`:180`）。

**GPUI 落点**：
- Ctrl+S：绑成 action，谓词用预览器自己的 `key_context`（不是 `Workspace`），
  否则会和别处抢；或者直接在预览器容器上 `on_key_down`。
  注意 gpui-component 的 `InputState` 在 code editor 模式下**自己会处理很多键**，Ctrl+S 不在其中。
- Esc：GPUI 里「Esc 只关最上层」是结构性免费的（`docs/gpui-parity-audit.md:71`）。
  但**两段式退出要自己接**：先问 gpui-component 搜索面板是否开着
  （`src/input/search.rs:305` 的 `on_action_escape` 会自己吃掉一次 Esc，
  实测确认它是否 `stop_propagation`；若是则两段式白拿，若否要自己判）。
- `confirmDiscard`：用 `crate::prompt` 的 confirm（`crates/mt-app/src/prompt.rs:155-211` 的
  `ConfirmBuilder`）。⚠️ 它是**命令式弹窗**，在 `open_guarded` 的 kind 表里是 `kind::CONFIRM`
  （`overlay.rs:58`），与预览器的 kind 不同种类 → 可以叠开（`prompt.rs:15-19` 明说这是合法的）。
- **遮罩点击**：原版 `Modal` 默认可点遮罩关闭。GPUI 的 `Dialog` 有 `.overlay_closable(bool)`
  （`search_modal.rs:125` 用了 `false`）。预览器有未保存内容的确认兜底，
  保持**可点遮罩**（原版语义）即可。

---

## B.8 弹窗外观

| 项 | 原版 | 出处 |
|---|---|---|
| 面板 | `w-[90vw] h-[80vh] select-text`，`align="center"` | `FileViewerModal.tsx:312-313` |
| 工具栏 | `flex justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0` | `:315` |
| 左区 | `←`(条件) + 文件类型图标(16px) + 文件名（`text-base font-medium text-[var(--accent)]`）+ 脏点 + 完整路径（`text-sm text-[var(--text-muted)] truncate`） | `:316-339` |
| 右区 | 保存按钮 + 预览/源码段控件 + `✕`（`text-lg`） | `:340-381` |
| 内容区 | `flex-1 overflow-auto bg-[var(--bg-base)]` | `:409` |
| 文件图标 | `resolveFileIcon(fileName, false)` → `<img class="mt-icon mt-icon-file w-4 h-4">` | `:309`、`:326-328` |

**GPUI 尺寸**：照 `search_modal.rs:110-129` 的写法算——
`width = viewport.width * 0.9`、`height = viewport.height * 0.8`、`margin_top = viewport.height * 0.1`。
**文件图标用 `mt_ui::FileIcon::new(name, false, false)`**（`crates/mt-ui/src/icons/file.rs:906-935`），
它已经把原版 53 类映射与「特殊文件名压扩展名」的语义搬过来了，`.size(px(16.0))`。

---

## B.9 GPUI 现状差异清单（做完本批要能逐条勾掉）

| # | 差异 | 处置 |
|---|---|---|
| 1 | 文件树单击文件无反应 | 改为开预览器（`file_tree.rs:675-681`） |
| 2 | 全局搜索单击 = 外部编辑器 | 改为「单击预览 / 双击编辑器」，删掉 `search_modal.rs:23-24` 与 `:255-256` 的偏差注释 |
| 3 | 无预览器模块 | 新增 `crates/mt-app/src/file_viewer.rs` + `overlay::kind::FILE_VIEWER` |
| 4 | gpui-component 无语言包 | 根 `Cargo.toml:39` 开 `features = ["tree-sitter-languages"]` |
| 5 | `--syn-*` 九色无映射 | 在 `crates/mt-ui/src/theme_bridge.rs` 补 `HighlightTheme` 映射 |
| 6 | HTML 预览无 iframe 等价物 | 建议只留源码态，记档（B.6.3） |
| 7 | SVG 图片红蓝互换 | 退到「使用默认工具打开」或等上游（B.2.3） |
| 8 | **CRLF 往返** | gpui-component 的 `InputState`/`ropey` 是否保留 `\r\n` **必须实测**：不保留就要在读入时记 `has_crlf`、写出时把 `\n` 还原成 `\r\n`（`CodeEditor.tsx:242-252` 是原版的处理）。**漏了这条，Windows 上改一个字就是整文件 diff。** |
| 9 | CodeMirror 面板中文 21 条 | gpui-component 的搜索面板走它自己的 `rust-i18n`（`ui.yml`），P 批已经把 `Locale::bcp47()` 桥过去了（见 commit `04ee62b`），**不需要再抄这 21 条**——现场确认面板文案是中文即可 |
| 10 | 文件树无键盘导航 | 入口 #2 跳过，记档 |
| 11 | 远程项目守卫 | 无远程项目，记档（属 #28） |

---

## B.10 i18n 对账

`fileViewer` 命名空间 **15 条全部已在** `crates/mt-i18n/src/dict.rs:306-322`(zh) / `:324-340`(en)：

```
back / binaryNotSupported / externallyChanged / loading / openWithDefaultApp / preview /
reloadDiscard / save / saveFailed / saving / source / tooLarge / unsaved / unsavedMessage / unsavedTitle
```

与 `src/i18n/locales/fileViewer.ts` 一字不差。

另需的一条：`externalLink.openConfirm`（`dict.rs:210-212` = "在浏览器中打开链接?" / "Open link in browser?"）✅。

**本节 i18n 缺口：零。** 硬编码字符串只有保存按钮的 `title="Ctrl+S"`（`FileViewerModal.tsx:350`），
原版就没走 i18n，照抄。

---

## B.11 B 节实现清单（14 条）

1. `crates/mt-app/src/file_viewer.rs` 新模块 + `overlay::kind::FILE_VIEWER` + `open_guarded` 接入。
2. 打开入口两处改线（文件树单击、全局搜索单击/双击），删旧偏差注释。
3. 读文件：`mt_project::fs::read_file_content` 走 `background_executor` 回主线程（**不要在主线程读盘**）。
4. 四种渲染分支（loading / error / image / binary+tooLarge / 编辑器），含 `canEdit` 判定。
5. 图片分支：位图 `gpui::img(Resource::Path)`；**svg 单独处置**；`ico/avif/tiff` 支持度实测。
6. 编辑器：`InputState::new(window,cx).code_editor(lang).line_number(true).soft_wrap(is_prose)`，
   语言按扩展名映射（对照 `LanguageDescription.matchFilename` 覆盖的常见类型）。
7. 开 `tree-sitter-languages` feature + `HighlightTheme` ← `--syn-*` 九色映射。
8. **CRLF 往返实测与兜底**（差异清单 #8）。
9. 三源状态机（`saved` / `draft` / `disk` + `dirty`）与保存流程（B.5.1），保存后按最新草稿重比对。
10. `FsWatcher` 接线 + 2s 回声窗口 + 干净静默重载 / 脏挂提示条两条路。
11. Markdown 预览（`TextView::markdown`）+ `.md-preview` 样式移植 + 三种链接处置（外链要确认框）。
12. 弹窗内跳转历史栈 + `confirmDiscard` 三处调用点。
13. Ctrl+S / Esc 两段式退出 / 遮罩关闭。
14. 工具栏（`FileIcon` + 文件名 + 脏点 + 路径 + 保存按钮 + 段控件 + ✕）与 90vw×80vh 尺寸。

---

# C. 三个最需要提前拍板的坑

1. **marker 锚点会静默漂移**（A.3）：alacritty 没有 xterm 的 `IMarker`，裸存 `Line(i32)`
   会随输出漂移；`anchor = line + history_size` 在 scrollback 未满时精确，但 10000 行装满后
   `history_size` 冻结、eviction 不再计数，marker 会指向**错误的行**而不是失效。
   必须显式选一条补路（文本重定位 / 饱和即剪枝），并写进注释。

2. **marker 功能在原版里大部分时候不产生任何数据**（A.0）：`registerAiMarker` 在 alt screen
   直接返回 null，而 Claude/Codex 这类 TUI 一进来就切备用缓冲区。做之前先接受
   「⚑ 按钮平时不出现」是**正确行为**，别为了「看起来有用」放开 alt screen 打点
   （alt grid 的 `max_scroll_limit` 是 0，没地方可跳）。

3. **CRLF 往返漏了就是整文件 diff**（B.9 #8）：原版专门用
   `EditorState.lineSeparator.of('\r\n')` 保住 Windows 行尾；gpui-component 的
   `InputState`/`ropey` 行为未验证。写回前必须实测一次「打开 CRLF 文件 → 改一个字 → 保存 → `git diff`」，
   不然 Windows 用户每保存一个文件就多一屏假 diff。

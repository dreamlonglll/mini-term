# Codex CLI 滚动相关历史处理复审

> 调研时间：2026-07-21
> 范围：`scrollback`、ED3（CSI 3J）、alternate screen（47/1047/1049）、
> focus/resize 导致的视口回底与状态误判，以及它们和本次便携 ConPTY 修复的关系。
> 方法：只读检查 Git commit/blame/diff、当前源码、当前任务研究资料，以及仓库锁定的
> xterm.js 6.0.0 随包源码；另用未修改仓库文件的 Node 小探针验证 xterm 默认行为。

## 结论

历史处理在当时都是针对真实现象的合理修补，但需要重新划分职责：

- **便携 ConPTY 是本次跨机器故障的根因修复**：它发生在 VT 字节流进入 xterm.js
  之前，用固定的 `conpty.dll` / `OpenConsole.exe` 消除 Windows build 差异。
- **100k scrollback、ED3 拦截和 alternate-screen 拦截是 xterm.js 侧的历史保留策略**：
  它们发生在 VT 字节流进入 xterm.js 之后，无法修复截图中“根本没有形成 scrollback”
  的问题。
- 两层代码**不重复、没有直接代码冲突，但会互相遮蔽验收结果**。若 handler 一直开启，
  “现在能滚动”不能单独证明便携 ConPTY 已经解决根因；需要在原故障机器做 A/B。
- 本次便携 ConPTY 修复**不应顺手删除 parser handler**，否则同时改变 PTY backend 和
  终端产品语义，无法归因，而且会撤销 README 已公开承诺的历史保留行为。
- 后续应把 ED3/alternate-screen 从“Codex 根因修复”改称为明确的
  `history-retention / TUI compatibility policy`。其中 alternate-screen 的全局拦截风险
  明显高于 ED3，必须优先收敛作用域、配置和测试。

建议矩阵：

| 历史处理 | 与便携 ConPTY 的关系 | 当前建议 | 后续目标 |
|---|---|---|---|
| `scrollback: 100000` | 互补 | **保留** | 可单独评估多 pane 内存占用，不属于本故障 |
| 吞掉 ED3（CSI 3J） | 同症状、不同层；不是根因修复 | **保留但重新定性** | 明确为“历史优先”策略；若要标准终端语义，改成可配置 |
| 吞掉 DECSET/DECRST 47/1047/1049 | 同症状、不同层；不是根因修复 | **本次先保留，随后必须调整** | 收敛为可测试、可配置的兼容模式；处理混合参数风险 |
| focus 事件不 `scrollToBottom` | 正交、互补 | **保留** | 维持精确序列匹配 |
| resize/focus 800ms 冷却 | 正交、互补 | **保留** | 注释改为通用 TUI viewport redraw；用测试/日志复核时长 |
| 普通用户输入时 `scrollToBottom` | 正交 | **保留** | 不把 PTY 输出或焦点事件误当用户输入 |

## 两层修复的边界

```text
Codex / shell
    │  控制台 API / VT 输出
    ▼
ConPTY + OpenConsole                 ← 本次便携运行时修复
    │  PTY 字节流
    ▼
pty-output → term.write(...)
    ▼
xterm.js parser / normal+alt buffer ← 既有 ED3 / alternate-screen 策略
    ▼
scrollback / scrollbar
```

当前链路证据：

- `src-tauri/src/lib.rs:43-48` 在 Tauri setup 第一项预载便携 ConPTY；
  `src-tauri/src/pty.rs:804-806` 随后才首次 `native_pty_system().openpty(...)`。
- `src/utils/terminalCache.ts:230-234` 将 `pty-output` 写入 `term.write(...)`；
  ED3 与 alternate-screen handler 位于其下游。
- 当前任务的 `research/pty-code-path.md:46-51` 也得出相同边界；PRD 把继续改写
  ED2/ED3、DECSTBM、SU/RI 列为 Out of Scope。

因此，旧 handler 不能修复 ConPTY 对控制序列的合成、透传或节奏差异；便携 ConPTY
也不会自动替代 xterm 侧“是否尊重 ED3”“是否允许备用缓冲区”的产品选择。

## Git 历史逐项复审

### 1. 100k scrollback：保留

- 提交：`46c3012bc44b51f881e816a3e3cb7ee287870781`
- 日期：2026-04-02
- 原问题：5000 行容量不足，长 AI 会话和构建日志的较早内容被容量上限淘汰。
- 原假设：只要主缓冲区确实产生历史行，提高容量就能延长可回溯范围。
- 当前实现：`src/utils/terminalCache.ts:284` 仍为 `scrollback: 100000`。

判断：该假设仍成立。这项配置只决定“已产生的历史最多保留多少”，不负责让
`normal.baseY` 从 0 增长。它没有解决截图中的根因，但和便携 ConPTY 完全互补。

### 2. ED3（CSI 3J）拦截：实现正确，但应重新定性

- 提交：`5d7a02a0bb66297230bface5675c6a448730b44a`
- 日期：2026-04-10
- 原问题：Codex/Claude 等 TUI 发送 ED3 后，xterm.js 清除 saved lines，用户无法回看。
- 原假设：ED3 是历史消失的直接原因；只吞掉参数 3，ED0/1/2 仍走默认行为。
- 当前实现：`src/utils/terminalCache.ts:301-305`：
  `registerCsiHandler({ final: 'J' }, params => params[0] === 3)`。

xterm.js 6.0.0 的一手证据：

- `node_modules/@xterm/xterm/src/common/InputHandler.ts:1174-1195` 明确定义 ED3 为
  `Erase scrollback`；`1248-1257` 会 trim viewport 之外的行并重置 `ybase/ydisp`。
- `node_modules/@xterm/xterm/typings/xterm.d.ts:1805-1817` 规定自定义 handler 返回
  `true` 即表示已处理，不再调用之前/默认 handler。
- 本次 Node 探针中，标准终端在 `CSI 3J` 后从 `baseY=1,length=4` 变为
  `baseY=0,length=3`；注册当前 handler 后仍为 `baseY=1,length=4`。

所以当前代码实现了它声称的行为，但这不是“修复错误的 xterm 行为”：xterm 正在严格
执行 ED3，mini-term 是有意选择“历史优先于发送方的清除请求”。普通 `clear` 或其他 TUI
若明确要求清历史，也会被全局阻止。

与本次故障的关系：截图和 PRD 已说明，即使围绕 ED3 做前端处理，受影响机器仍可能
`normal.baseY = 0`；没有历史行时，保护 ED3 也无内容可保护。因此它是有价值的产品
策略，但不能再作为跨 Windows 滚动问题的根因解释。

建议：本次先保留，因为 README 已公开承诺“清屏时保留上滚历史”，且同时删除会让
便携 ConPTY 验收失去单变量条件。后续把它命名、注释和 spec 统一为历史保留策略；若
项目希望恢复标准终端语义，则提供显式开关，而不是静默改变。

### 3. alternate-screen 拦截：短期保留，优先调整

- 提交：`bcebffaa630472a189e369211518793d9cc5e718`
- 日期：2026-05-07
- 原问题：备用缓冲区按设计没有 scrollback，Codex 等 TUI 进入备用屏后看不到历史。
- 原假设：吞掉 DECSET/DECRST 47、1047、1049，让全部绘制留在 normal buffer，即可
  “始终”获得 scrollbar 和 scrollback。
- 当前实现：`src/utils/terminalCache.ts:309-320` 全局拦截两个 CSI identifier；
  `params.some(isAltScreenMode)` 只要发现一个目标参数就返回 `true`。
- README 中英文当前仍把这一行为列为正式功能；中文位于 `README.zh-CN.md:48`。

该策略有合理动机：xterm.js 6.0.0 的 `BufferSet.ts:99-113` 说明 alternate buffer 是
独立活动缓冲区；切回 normal 时会清空 alternate。默认允许 1049 后，备用屏应用本来就
不会像普通 shell 输出一样积累 scrollback。便携 ConPTY 不会改变这一终端模型。

但原假设“留在主缓冲区就一定形成 scrollback”不完整：

- xterm `InputHandler.ts:1422-1437` 的 SU 实现只在滚动区域内 splice 行，源码还保留
  `scrolled out lines ... should add to scrollback` 的 FIXME。
- 本次探针验证：normal buffer 中执行 `CSI 1S` 后 `baseY` 仍为 0、长度仍为 3；普通
  底部换行才使 `baseY` 增至 1、长度增至 4。
- 这正好解释为什么 Codex 使用 DECSTBM + SU/RI 重绘时，即便禁止 alternate screen，
  仍可能没有 scrollback。

当前实现还有两个协议风险：

1. 1049 的标准行为不仅是换 buffer，还包含保存/恢复光标。xterm 的
   `InputHandler.ts:1956-1967`、`2184-2198` 会执行这些状态转换；当前代码整段吞掉。
2. DECSET/DECRST 支持一条 CSI 带多个参数，xterm 默认实现逐个处理；但当前
   `params.some(...)` 在例如 `CSI ?1004;1049h` 中会把整条序列判为已处理，导致 1004
   这样的无关模式也不生效。公开 parser API 没有“只消费其中一个参数”的返回协议。

建议：本次便携 ConPTY 合入时先保留，避免改变已公开产品行为；但随后单独建任务，把它
收敛为有名称、有开关、有测试的 TUI 兼容模式。最低限度要覆盖单参数、非 alt 参数及混合
参数；混合参数无法无损部分消费时，应明确选择整条放行或实现更深的状态适配，不能继续
静默吞掉无关模式。若原故障机器 A/B 证明便携 ConPTY 下 Codex 不再依赖此策略，则优先
恢复标准 alternate-screen 语义。

### 4. 用户输入自动回底与 focus 例外：保留

- 基础提交：`20ce5f9664265022fa2b7eac6b3af1fca6233b86`（2026-03-30），用户输入时
  调 `scrollToBottom`，保证键入后看到最新内容。
- 修正提交：`3f60bc8c6ae80128530fb4d40ff0f69a214a4108`（2026-04-23），识别 xterm
  sendFocus 模式产生的 `CSI I` / `CSI O`，这两种非用户输入不再把视口拉到底部。
- 当前实现：`src/utils/terminalCache.ts:249-251,358-369`；精确等值匹配后才跳过回底，
  焦点序列仍正常写入 PTY，TUI 仍可感知焦点。

判断：这是确定的前端交互 bug 修复，不依赖 ConPTY 版本。便携 ConPTY 不会改变
`term.onData` 中由 xterm 自己产生的 focus 数据；应保留。

### 5. resize/focus 重绘冷却：保留，修正文案

- resize 提交：`bfda4d4aeaa8191dd0a5cc865fdbb2ef86dd2382`（2026-04-11）。
- focus 扩展：`3f60bc8c6ae80128530fb4d40ff0f69a214a4108`（2026-04-23）。
- 原问题：resize 或焦点事件让 TUI 重绘；后端把重绘输出刷新为 `last_output`，进而把
  idle 误判为 working，并触发假的“任务完成”通知。
- 当前实现：`src-tauri/src/pty.rs:398-420,1102-1110` 用每 PTY 800ms 冷却窗口屏蔽
  这类重绘对活动时间戳的影响；历史提交同时增加了严格匹配、max 语义和隔离测试。

判断：它处理的是 AI 状态启发式，不是 scrollback 形成问题。新版 ConPTY 仍会传递
resize/focus，TUI 仍会重绘，因此应保留。便携运行时可能改变重绘时序，800ms 仍是经验
值，受影响机器验收时应观察是否足够，但不能因为更换 backend 就直接删除。

需要调整的只是注释：当前多处写“重绘 Alternate Screen Buffer”，而 mini-term 又全局
阻止 xterm 进入 alternate buffer。更准确的说法是“TUI viewport/full-screen redraw”；
逻辑本身不依赖 xterm 当前 active buffer 类型。

### 6. split/remount 视口恢复：保留

提交 `2ef0045e185d17b5ee6be11841ad670dc7f34344`（2026-04-16）在 split/remount、
fit/refresh 后按原先是否位于底部恢复视口。这是布局生命周期的通用修复，与 Codex 控制
序列及 ConPTY backend 都正交，不应在本次清理中移除。

## 未进入仓库的排查方案

对全历史执行 `git log -G` 检查后：

- 没有业务代码提交过 `--no-alt-screen`；
- 没有业务代码提交过 `TERM=dumb`；
- 没有提交过 DECSTBM 或 SU/RI 的前端改写。

所以用户路线图中的这些内容属于调查阶段尝试/回滚，并非当前 Git 历史遗留。当前唯一
持续存在的 ANSI 改写就是 ED3 与 DECSET/DECRST 47/1047/1049 两组 parser handler。

## README 与实现表述需要校正

`README.zh-CN.md:48` 当前宣称“滚动条和 scrollback 始终可用”。结合 xterm 的 SU
实现和本次实际故障，这个承诺过强。建议在后续改为两层表述：

1. 100k + ED3/alternate compatibility policy 尽力保留 TUI 历史；
2. Windows 打包固定版 ConPTY/OpenConsole，以减少系统版本差异；
3. 不承诺任意 TUI 的任意滚动区域重绘都能转换成 scrollback。

这样能避免再次把“容量”“历史保留策略”和“PTY 字节流正确性”混成同一个问题。

## 受影响机器上的 A/B 验收

在删除或永久保留 parser handler 之前，至少应在原故障机器和一台原本正常的机器各跑：

| 组别 | ConPTY | ED3 handler | alt handler | 目的 |
|---|---|---:|---:|---|
| A | portable | 开 | 开 | 当前目标基线，先确认用户问题解决 |
| B | portable | 关 | 开 | 判断 ED3 策略是否仍影响 Codex 历史 |
| C | portable | 开 | 关 | 判断 Codex 是否进入 alt、退出恢复与 scrollback 表现 |
| D | system fallback | 开 | 开 | 确认旧机器原问题仍可归因于系统 backend |

每组至少记录：

- `[conpty-bootstrap] backend=portable/system`；
- 实际 `OpenConsole.exe` 路径/版本；
- `term.buffer.active.type`、`normal.baseY`、`active.baseY`；
- 是否观察到 ED2/ED3、47/1047/1049、DECSTBM、SU/RI；
- Codex 长输出能否滚动、历史命令能否回看、退出 TUI 后主屏/光标是否正确恢复。

在 A 通过前不要讨论删除旧 handler；在 B/C 同时覆盖故障机和正常机前，也没有足够证据
声称某个 handler 已冗余。

## 证据索引

- Git commits：`46c3012`、`5d7a02a`、`bfda4d4`、`2ef0045`、`3f60bc8`、
  `bcebffa`。
- Git blame：`src/utils/terminalCache.ts:284,301-320,358-369`；
  `src-tauri/src/pty.rs:398-420,1102-1110`。
- 当前任务：`prd.md`、`research/pty-code-path.md`、`research/portable-conpty.md`、
  `verification.md`。
- xterm.js 6.0.0 随包源码：`InputHandler.ts`、`BufferSet.ts`、`typings/xterm.d.ts`；
  版本由 `package-lock.json` / `npm ls @xterm/xterm --depth=0` 确认为 6.0.0。

## 未确认项

- 原故障机器尚未完成上述 A/B；当前自动化只能证明便携资源、加载选择和回退契约，
  不能证明某个前端 handler 已可删除。
- 尚未捕获一份原故障机器与正常机器的原始 VT 序列用于逐字节比较；对 ED3、
  DECSTBM、SU/RI 的具体组合仍以用户路线图和现有源码行为为证。
- 未确认 Codex 当前版本是否会发出包含 1049 与其他 DEC 私有模式的混合 CSI；风险来自
  xterm parser API 与当前 handler 的确定语义，但仓库没有已知复现样本。

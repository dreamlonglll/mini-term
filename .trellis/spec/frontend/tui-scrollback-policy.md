# TUI 历史保留与 Windows PTY 分层契约

> mini-term 为 AI/TUI 会话选择“历史优先”体验，但该策略必须与 Windows ConPTY
> 字节流兼容性分层维护，不能把下游 parser handler 当成上游 PTY 根因修复。

## 1. Scope / Trigger

修改以下任一位置时必须检查本规范：

- `src/utils/terminalCache.ts` 的 `scrollback`、CSI parser handler、`scrollToBottom`；
- ED3（CSI 3J）或 DECSET/DECRST 47、1047、1049 处理；
- `src-tauri/src/pty.rs` 的 resize/focus 重绘冷却；
- Windows ConPTY/OpenConsole backend 或 xterm.js 版本。

数据流分层固定为：

```text
Codex/TUI → ConPTY/OpenConsole → pty-output → term.write → xterm parser/buffer
             上游兼容层                         下游历史保留策略
```

## 2. Signatures

当前下游策略的关键入口：

```ts
new Terminal({ scrollback: 100000 });

term.parser.registerCsiHandler(
  { final: 'J' },
  params => params[0] === 3,
);

term.parser.registerCsiHandler(
  { final: 'h', prefix: '?' },
  params => params.some(isAltScreenMode),
);
term.parser.registerCsiHandler(
  { final: 'l', prefix: '?' },
  params => params.some(isAltScreenMode),
);
```

`registerCsiHandler` 返回 `true` 表示整条 CSI 已被处理，xterm 默认 handler 不再执行；
公开 API 不支持“只消费一条 CSI 中的部分参数”。

## 3. Contracts

- `scrollback: 100000` 只控制已经形成的 normal-buffer 历史容量，不负责让 `baseY` 增长。
- ED3 handler 是有意覆盖标准 `Erase Saved Lines` 语义的“历史优先”产品策略；ED0/1/2
  必须继续交给 xterm 默认实现。
- 47/1047/1049 handler 是全局 TUI 兼容策略，不是 Codex 专用开关；它让输出留在 normal
  buffer，但同时放弃标准 alternate-buffer 及 1049 保存/恢复光标语义。
- focus 产生的 `CSI I`/`CSI O` 仍写入 PTY，但不得触发 `scrollToBottom`。
- resize/focus 冷却只抑制 TUI viewport/full-screen redraw 对 AI 活动时间戳的污染；不得称为
  alternate-buffer 专属逻辑。
- 便携 ConPTY 负责保证进入 xterm 前的 PTY 字节流行为跨 Windows build 一致，不替代上述
  历史保留策略；上述策略也不能修复 `normal.baseY` 从未形成的上游问题。

## 4. Validation & Error Matrix

| 条件 | 必须行为 |
|---|---|
| CSI J 参数为 3 | 当前策略吞掉 ED3，保留 saved lines |
| CSI J 参数为 0/1/2 | 返回 `false`，执行 xterm 默认擦除语义 |
| DECSET/DECRST 仅含 47/1047/1049 | 当前兼容策略吞掉整条序列 |
| DECSET/DECRST 不含 alt 参数 | 返回 `false`，不得影响其他 DEC 私有模式 |
| 混合参数（如 `?1004;1049h`） | 当前实现会吞掉整条，属于已知风险；后续调整必须显式测试和记录取舍 |
| focus 输入 `CSI I/O` | 写入 PTY，但保持用户当前滚动位置 |
| 便携 ConPTY 资源缺失/回退 | parser 策略保持不变，不能用 handler 开关掩盖 backend 诊断 |

## 5. Good / Base / Bad Cases

- **Good**：便携 ConPTY 提供稳定字节流；ED3 handler 保留已形成的历史；用户输入回到底部，
  focus 切换不改变手动上翻位置。
- **Base**：标准 `clear` 明确发送 ED3，但 mini-term 仍保留历史。这是已公开的“历史优先”
  取舍，不应描述成 xterm bug。
- **Bad**：看到 Codex 可滚动就宣称 ConPTY 根因已修复，却没有记录 backend；或在同一改动中
  同时更换 ConPTY 并删除 parser handler，导致结果无法归因。
- **Bad**：用 `params.some(...)` 吞掉含 1049 的混合序列，却忽略同一序列中的 1004 等模式。

## 6. Tests Required

任何改动 ED3/alternate-screen 策略的任务至少覆盖：

1. ED3 被处理，ED0/1/2 被放行；
2. 单独 47/1047/1049 的 set/reset；
3. 非 alternate DEC 私有模式被放行；
4. 混合 DEC 参数的行为是明确断言，不得静默随实现漂移；
5. focus `CSI I/O` 不调用 `scrollToBottom`，普通用户输入仍调用；
6. 原故障机器执行 portable/system backend 与 handler 开/关 A/B，记录 active buffer、
   `normal.baseY`、控制序列和实际 OpenConsole 路径。

在 portable backend 基线通过、且故障机与正常机的 handler A/B 都有证据前，不得删除现有
parser handler。

## 7. Wrong vs Correct

### Wrong：把 ED3 拦截当成 Windows 根因修复

```ts
// 只能处理已经到达 xterm 的 ED3，不能修复 ConPTY 未形成历史行。
term.parser.registerCsiHandler({ final: 'J' }, params => params[0] === 3);
```

### Correct：分别诊断 backend 与历史策略

```text
[conpty-bootstrap] backend=portable ...
term.buffer.active.type = normal
term.buffer.normal.baseY = <observed>
ED3 handler = on/off
alternate handler = on/off
```

先证明固定 ConPTY backend 解决跨机器差异，再独立决定是否保留历史优先策略。

### Wrong：无意识吞掉混合 DEC 参数

```ts
params.some(isAltScreenMode); // ?1004;1049h 会连 1004 一起吞掉
```

### Correct：把混合参数作为显式兼容决策

若公开 parser API 无法部分消费，必须通过测试明确选择“整条放行”或更深的状态适配；不可
继续把无关模式丢失当成未定义行为。

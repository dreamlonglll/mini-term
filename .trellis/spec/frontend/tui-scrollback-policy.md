# TUI scrollback 与 Windows PTY 分层契约

> mini-term 使用 10 万行 normal-buffer 容量并遵循标准 ED3 清历史语义；
> alternate-screen 47/1047/1049 仍是独立的全局兼容策略。两者必须与上游 Windows
> ConPTY 字节流兼容性分层维护。

## 1. Scope / Trigger

修改以下任一位置时必须检查本规范：

- `src/utils/terminalCache.ts` 的 `scrollback`、CSI parser handler、`scrollToBottom`；
- ED3（CSI 3J）或 DECSET/DECRST 47、1047、1049 处理；
- `src-tauri/src/pty.rs` 的 resize/focus 重绘冷却；
- Windows ConPTY/OpenConsole backend 或 xterm.js 版本。

数据流分层固定为：

```text
Codex/TUI → ConPTY/OpenConsole → pty-output → term.write → xterm parser/buffer
             上游兼容层                         下游终端语义
```

## 2. Signatures

当前下游策略的关键入口：

```ts
new Terminal({ scrollback: 100000 });

// 不注册 CSI J override：ED0/1/2/3 全部由 xterm 默认 handler 执行。

term.parser.registerCsiHandler(
  { final: 'h', prefix: '?' },
  params => params.some(isAltScreenMode),
);
term.parser.registerCsiHandler(
  { final: 'l', prefix: '?' },
  params => params.some(isAltScreenMode),
);
```

`registerCsiHandler` 返回 `true` 表示整条 CSI 已处理，xterm 默认 handler 不再执行；
公开 API 不支持“只消费一条 CSI 中的部分参数”。

## 3. Contracts

- `scrollback: 100000` 只控制已经形成的 normal-buffer 历史容量，不保证历史永不被应用清除。
- 不得注册 CSI J override。ED0/1/2/3 全部进入 xterm 默认实现；其中 ED3 标准语义是
  `Erase Saved Lines`，可永久清除 saved lines。
- ED3 是全局终端语义：Codex、Claude、shell `clear` 与其他应用一视同仁。parser callback
  没有可靠发送者身份，不得用 ANSI 指纹、AI 状态或进程启发式做“仅 Codex”放行。
- Codex inline consolidation 会发送 `ED2 + ED3`，清除流式临时 scrollback 后从 canonical
  transcript 重放；吞掉 ED3 会留下已经进入 saved lines 的旧临时内容。
- 47/1047/1049 handler 仍是全局 TUI 兼容策略，与 ED3 独立；它让 overlay 输出留在
  normal buffer，但放弃标准 alternate-buffer 及 1049 保存/恢复光标语义。
- focus 产生的 `CSI I`/`CSI O` 仍写入 PTY，但不得触发 `scrollToBottom`。
- resize/focus 冷却只抑制 TUI viewport/full-screen redraw 对 AI 活动时间戳的污染。
- 便携 ConPTY 负责保证进入 xterm 前的 PTY 字节流跨 Windows build 一致，不替代上述
  xterm 语义；放行 ED3 也不能修复 `normal.baseY` 从未形成的上游问题。

## 4. Validation & Error Matrix

| 条件 | 必须行为 |
|---|---|
| CSI J 参数为 0/1/2/3 | 不由 mini-term 消费，执行 xterm 默认擦除语义 |
| Codex `ED2 + ED3 + replay` | 删除旧 saved lines，只留下重放后的 canonical transcript |
| shell/Codex `/clear` 发送 ED3 | 可真正清除旧 scrollback，这是有意的标准语义 |
| DECSET/DECRST 仅含 47/1047/1049 | 当前兼容策略吞掉整条序列 |
| DECSET/DECRST 不含 alt 参数 | 返回 `false`，不得影响其他 DEC 私有模式 |
| 混合参数（如 `?1004;1049h`） | 当前实现吞掉整条，属于已知风险；后续调整必须显式测试 |
| focus 输入 `CSI I/O` | 写入 PTY，但保持用户当前滚动位置 |
| 便携 ConPTY 资源缺失/回退 | xterm parser 策略不变，不能用 handler 开关掩盖 backend 诊断 |

## 5. Good / Base / Bad Cases

- **Good**：便携 ConPTY 提供稳定字节流；Codex hard-reset 的 ED3 删除 transient saved
  lines，最终只看到 canonical transcript；普通历史仍可增长到 10 万行。
- **Base**：应用明确发送 ED3 后，用户无法再上翻已 purge 的内容。这是恢复标准终端语义的
  产品取舍，不是 xterm 数据丢失 bug。
- **Bad**：保留 CSI J handler 并声称“只保护历史”；这会破坏 Codex consolidation、
  `/clear`、resize/线程切换后的 canonical replay。
- **Bad**：为“仅 Codex 放行”猜测发送者，或顺手删除 alternate-screen handler，导致两个
  独立产品语义被绑定在一次改动中。
- **Bad**：用 `params.some(...)` 吞掉含 1049 的混合序列，却忽略同一序列中的 1004 等模式。

## 6. Tests Required

任何改动 ED3/alternate-screen 策略的任务至少覆盖：

1. 在真实 xterm 上写入 Codex 同形 `ED2 + ED3 + folded transcript`，断言旧 saved lines
   消失、`baseY == 0` 且 canonical transcript 存在；
2. `terminalCache.ts` 不再注册 CSI J handler，因而 ED0/1/2/3 均由 xterm 默认实现；
3. `scrollback: 100000` 保持；
4. 单独 47/1047/1049 的 set/reset handler 保持；
5. 非 alternate DEC 私有模式被放行；混合 DEC 参数行为有明确断言；
6. focus `CSI I/O` 不调用 `scrollToBottom`，普通用户输入仍调用；
7. 原故障机器记录 portable/system backend、active buffer、`normal.baseY`、控制序列和
   实际 OpenConsole 路径。

## 7. Wrong vs Correct

### Wrong：吞掉 ED3 保护历史

```ts
term.parser.registerCsiHandler({ final: 'J' }, params => params[0] === 3);
```

ED2 只能清可见区，已经进入 saved lines 的 transient 内容仍残留，阻止 Codex
hard-reset/replay 完成自动折叠。

### Correct：恢复 xterm 标准 ED3，独立保留 alternate-screen 策略

```ts
const term = new Terminal({ scrollback: 100000 });
// 无 CSI J override。

term.parser.registerCsiHandler(
  { final: 'h', prefix: '?' },
  params => params.some(isAltScreenMode),
);
```

### Wrong：通过 ANSI/进程启发式仅对 Codex 放行

parser callback 只能看到通用 CSI 参数；同一 PTY 还会依次运行 shell、Codex 和其他程序。
若未来需要“禁止应用清历史”，应提供显式终端语义设置，而不是猜发送者身份。

# Codex CLI 自动折叠与终端控制序列

> 调研时间：2026-07-21
> 目标：确认 mini-term 的哪一项 ANSI 覆盖阻止 Codex CLI 自动折叠，并比较全局、
> Codex 专属和设置开关三种作用域。
> 方法：只读检查 mini-term 当前实现与 Git 历史；核对本机 Codex CLI 帮助；只读检查
> OpenAI Codex `rust-v0.144.6` 精确 tag；用未修改仓库文件的 Node/xterm.js 探针验证
> ED3 和 alternate-screen buffer 语义。

## 结论

用户所说的“普通聊天内容完成后自动折叠/把流式临时内容替换为最终内容”，其关键开关是
**ED3（CSI 3J，Erase Saved Lines）**，不是 alternate-screen：

- Codex 0.144.6 的普通聊天主界面运行在 inline viewport，并把最终聊天记录写入终端的
  normal scrollback。
- 流式回复完成后，Codex 会把一组 transient `AgentMessageCell` 合并为单个 canonical
  `AgentMarkdownCell`。需要重建时，它先发送 `ED2 + ED3` 清除可见区和旧 scrollback，
  再从 canonical transcript cells 重放最终内容。
- mini-term 当前在 `src/utils/terminalCache.ts:301-305` 吞掉 ED3。ED2 仍会清可见区，
  但已经进入 saved lines 的展开/临时内容无法删除，所以会表现成“最终内容画出来了，旧的
  展开内容却仍留在上滚历史中”，即用户观察到的“不自动折叠”。
- `src/utils/terminalCache.ts:309-320` 对 47/1047/1049 的拦截只阻止 Codex 进入
  alternate buffer。Codex 当前只在 transcript、diff、全屏审批、resume picker 等 overlay
  进入 alternate screen；单独放开 1049 不能恢复普通聊天折叠。

因此，**满足本次需求的最小语义变化是让 ED3 进入 xterm.js 默认 handler**。是否同时恢复
1049，应按“是否需要 Codex 原生全屏 overlay”另行决定，不能把二者绑成同一个技术结论。

## 本机版本和一手来源

- `codex --version`：`codex-cli 0.144.6`。
- `codex.exe`：
  `C:\Users\12197\AppData\Local\Programs\OpenAI\Codex\bin\codex.exe`，
  SHA-256 `4B76DED066D0239115CA97473D010C92072BC5C5550A45DD7CBEBE1E9EB956A7`。
- `codex --help` 对 `--no-alt-screen` 的定义是：禁用 alternate screen，以 inline mode
  运行并保留 terminal scrollback。
- 对应官方源码 tag：`rust-v0.144.6`，commit
  [`5d1fbf26c43abc65a203928b2e31561cb039e06d`](https://github.com/openai/codex/tree/5d1fbf26c43abc65a203928b2e31561cb039e06d)。

精确版本源码证据：

- [`tui/src/cli.rs:68-72`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/cli.rs#L68)
  定义 `--no-alt-screen`。
- [`tui/src/lib.rs:1845-1857`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/lib.rs#L1845)
  说明 0.144.6 默认允许 alternate screen，`--no-alt-screen` 或配置 `never` 才禁用。
- [`tui/src/tui.rs:734-769`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/tui.rs#L734)
  只在 overlay 请求时进入/退出 alternate screen。

## Codex 中三种容易混称为“折叠”的行为

### 1. 命令输出截断：Codex 内部渲染，不由 terminal handler 控制

Codex 会将普通 tool call 输出限制为 5 行、user shell 输出限制为 50 行，并在过长时保留
头尾、插入 `… +N lines`。证据见
[`exec_cell/render.rs:33-35,103-175`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/exec_cell/render.rs#L33)。
这项折叠不依赖 ED3 或 1049，mini-term 的两个 parser handler 不会直接禁用它。

### 2. Reasoning summary：Codex 内部 history cell，不由 alternate-screen 控制

reasoning block 结束后，Codex 构造 `ReasoningSummaryCell`；源码注释明确称为
`collapsed reasoning block`。证据见
[`history_cell/messages.rs:495-529`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/history_cell/messages.rs#L495)。

### 3. 流式消息 consolidation/reflow：依赖 ED3 清旧历史后重放

这是最符合“项目禁止了自动折叠”的路径：

1. [`app/agent_message_consolidation.rs:1-8,23-65`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/app/agent_message_consolidation.rs#L1)
   说明流式期间使用 transient cells，完成后将它们 fold 成单个 source-backed cell。
2. [`chatwidget/streaming.rs:19-49`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/chatwidget/streaming.rs#L19)
   在仍有 live tail 时请求 `ConsolidationScrollbackReflow::Required`。
3. [`app/resize_reflow.rs:235-240`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/app/resize_reflow.rs#L235)
   说明 inline 模式会清 scrollback 后重放；alt overlay 活跃时只清可见区。
4. [`custom_terminal.rs:535-551`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/custom_terminal.rs#L535)
   给出精确 hard-reset 序列：
   `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H`。

该 ED3 路径也服务 Codex `/clear`、线程切换、resize/rollback。放行 ED3 不是只恢复一种
动画，而是恢复 Codex 对其 inline transcript 的完整“清旧状态后按 canonical source 重放”
契约。

## 为什么不是 alternate-screen

Codex 0.144.6 普通聊天 history 使用 terminal scrollback；alternate screen 主要是 overlay：

- transcript：[`app/input.rs:167-173`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/app/input.rs#L167)；
- diff：[`app/event_dispatch.rs:450-458`](https://github.com/openai/codex/blob/5d1fbf26c43abc65a203928b2e31561cb039e06d/codex-rs/tui/src/app/event_dispatch.rs#L450)；
- 全屏审批：同文件 `2076-2139`。

本地 xterm.js 6.0.0 探针也验证了当前 mini-term handler 的实际语义：

```text
默认 1049：normal("normal") → alternate("ALT") → normal("normal")
拦截 1049：始终 normal，结果为 "normalALT"
```

所以该 handler 确实破坏 Codex overlay 的隔离和恢复，但它不是普通聊天 consolidation 的
必要条件。若用户只要求“允许自动折叠”，不应借机同时删除 alternate handler；若用户要求
“完整恢复 Codex 原生 TUI”，则两组 handler 都需要在所选作用域内放行。

## 最小 ED3 探针

探针使用仓库锁定的 `@xterm/xterm@6.0.0`，先写四行展开内容，再写 Codex 同形的
`ED2 + ED3 + folded` 序列：

```text
不拦截 ED3：before baseY=1 [expanded-1..4]
              after  baseY=0 [folded, "", ""]

拦截 ED3：  before baseY=1 [expanded-1..4]
              after  baseY=1 [expanded-1, folded, "", ""]
```

ED2 清掉了可见的 expanded 行，但只有放行 ED3 才能删除已经进入 saved lines 的旧内容。
这与 Codex hard-reset/replay 源码路径完全一致。

## 三种作用域方案

### 方案 A：全局恢复 ED3 标准语义

做法：删除/停用 `registerCsiHandler({ final: 'J' }, ...)`，让所有 PTY 的 ED3 走 xterm
默认行为；暂不改 1049 handler。

可行性：**高，改动和测试最小**，且直接满足 Codex 自动折叠。

风险：

- 撤销提交 `5d7a02a` 的公开“历史优先”策略；Codex、Claude、shell `clear` 及其他应用
  都能真正清除 saved lines。
- 用户无法再通过滚动条回看已被发送方明确 purge 的内容。这是恢复标准终端语义的必然
  结果，不是 xterm 数据丢失 bug。
- 与 alternate-screen 拦截形成“ED3 标准、1049 非标准”的混合策略；技术上可行，但文档
  必须明确两者独立。

建议测试：ED0/1/2/3 全部走 xterm 默认；Codex 完成流式回答后旧临时行消失；`/clear`、
resize、线程切换能正确重放；Claude/普通 `clear` 的历史清除行为作为有意变更验收；现有
100k scrollback 容量不变。

### 方案 B：仅 Codex 放行 ED3

做法：handler 根据当前 PTY 的前台应用身份，仅对 Codex 返回 `false`，其余应用继续吞
ED3。

可行性：**当前架构下不能可靠自动实现**。

原因：

- parser callback 只能看到 CSI 参数，没有发送者身份；ED3/ED2/DECSTBM/1049 都是通用
  ANSI，Codex 的组合也不是公开唯一指纹。
- `terminalCache.ts` 现有 `aiPtyIds` 只区分“AI/非 AI”，不区分 Codex/Claude；状态由
  hook/进程启发式异步更新，Codex 启动早期、hook 缺失、远程会话和进程退出都存在窗口。
- 同一个 PTY 先运行 shell、再运行 Codex、退出后又运行普通程序。仅记录 pane 的初始 shell
  或曾经见过 Codex 都会错误扩大放行范围。
- OSC title 可配置、可禁用、可伪造；不适合用作安全或协议语义的强身份。

可靠实现需要新增显式边界，例如专用 Codex profile/启动入口、给 PTY 生命周期附带
`terminalSemantics=codex-native`，或持续可靠跟踪前台进程的规范路径。后者还要处理 wrapper、
SSH/WSL、Codex 升级路径和崩溃清理，成本明显高于本需求。

建议测试：如果未来引入显式 Codex profile，应覆盖启动前首个 ED3、Codex 退出后恢复
保护、同 pane 依次运行 Codex/普通程序、Claude 不误命中、远程/WSL 行为和进程异常退出。

### 方案 C：设置开关

做法：提供明确设置，例如“允许应用清除终端历史（支持 Codex 原生折叠）”，在创建
Terminal 时决定是否注册 ED3 handler；ED3 与 alternate-screen 最好是两个独立策略项。

可行性：**高，且无需猜测前台应用身份**。

风险：

- 设置名称若只写“自动折叠”，用户难以理解 `/clear`、resize/replay 也会受影响；文案必须
  说明“关闭历史保护后，应用可永久清除 scrollback”。
- 如果是全局设置，切换后对已有 Terminal 是否立即生效要有明确契约。最简单可靠的实现是
  “仅新建终端生效”，并在 UI 提示；若要求即时生效，必须保存/dispose parser handler。
- 不应把 ED3 和 1049 隐式绑在一个布尔值里，否则用户只想恢复折叠时会同时改变 overlay、
  光标保存恢复和滚动体验。

建议测试：开关两态的 ED3 探针；持久化/默认值/旧配置迁移；已有与新建 terminal 生效时机；
Codex 折叠、`/clear`、resize、线程切换；开关不改变 1049 行为；配置切换不重复注册 handler。

## 明确建议

**推荐方案 C：把 ED3 历史保护改成显式设置，并让新默认值放行 ED3，以满足“允许 Codex
自动折叠”的当前产品要求；alternate-screen 拦截先保持不变。**

理由：它直接恢复 Codex consolidation/reflow，又给仍需要“任何应用都不能 purge 历史”的
用户保留兼容选项；相比“仅 Codex”不依赖不可靠的进程/ANSI 猜测。若当前迭代不希望增加
设置 UI，则方案 A 是正确的最小实现。

若产品目标进一步升级为“Codex 完整原生 TUI”，再单独决定是否同时放行 47/1047/1049；
这会恢复 transcript/diff/approval overlay，但也会重新引入 alternate buffer 无 scrollback、
退出恢复和混合 DEC 参数等既有取舍。

## Git 历史对应关系

- `5d7a02a`：全局吞 ED3，正是阻止 Codex purge/replay 的处理。
- `bcebffa`：全局吞 47/1047/1049，只影响 alternate-screen/overlay，非本次自动折叠主因。
- `85b67f5`：便携 ConPTY，修复 VT 字节流进入 xterm 之前的 Windows 兼容层；与是否尊重
  ED3 是正交决策。
- `2d7c3a6`：当前 spec 仍把 ED3 定义为“历史优先”产品策略。实现若改为默认放行，必须
  同步更新 `.trellis/spec/frontend/tui-scrollback-policy.md` 和 README 的公开承诺。

## 未确认项

- 未在用户原先观察到“不折叠”的真实会话上抓取原始 VT 字节流；结论来自精确版本 Codex
  源码、当前 mini-term handler 与 xterm.js 探针，链路证据完整，但仍建议实现后录制一次真实
  Codex 长回复验证。
- 用户口中的“折叠”若特指命令输出 5 行截断或 reasoning summary，它们是 Codex 内部
  history-cell 渲染，与 ED3/1049 不同；验收时应分别观察，避免把三个行为继续混称。

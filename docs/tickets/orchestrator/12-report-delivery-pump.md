# 12 — 汇报投递泵：投递闸 + 渲染 + 写穿编排者 + 桌面接线

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** 把 11 的账本装进 `ControlPlane`，加一条后台投递线程：编排者可投递（`ai-idle` 且非 attention 且 pane 活着）时把它收件箱里的汇报取空、渲染成一段文字、经既有的 `OrchestratorActions::send_input` 写进编排者的终端。桌面侧只做两处接线：状态变化与 pane 关闭喂给控制面。做完这张票，编排者派活之后**什么都不用做**，结果会自己回到它的对话里。

**Blocked by:** 10（任务账本）、11（汇报账本）

**Status:** todo

- [ ] `ControlPlane` 新增 `pub fn observe_status(pty_id, status, cause)` 与 `pub fn observe_pane_closed(pty_id)`：按记账表把「这个 pty 是谁的乐手」翻出来喂给 11 的账本；pty 是编排者自己时只用来**唤醒投递**（它可能刚变空闲）；两者都不是就忽略。记账表增删（`register_landed` / `revoke_pane` / 离场）同步到账本的 `register_hand` / `forget_hand`；编排者撤销令牌时 `drop_inbox`
- [ ] 投递闸 `can_deliver(&PaneLiveness) -> bool`（纯函数、有单测）：`alive && status == "ai-idle" && !is_attention_cause(cause)`。`ai-working` 暂存、黄灯暂存、`idle`（裸 shell）暂存、pane 没了丢弃
- [ ] 投递线程：`ControlPlane` 内一条常驻 `std::thread`（首次 `set_actions` 时起，与 hook 服务同寿），吃一条 `mpsc` 唤醒通道 + 每 1s 一次的自唤醒（跑 `tick` 与重试被暂存的收件箱）。唤醒时对每个有积压的编排者：查闸 → `take_batch` → 渲染 → `send_input(orchestrator, PaneInput::assemble(text))`（**不追加**格式尾部）。`send_input` 答 `PaneGone` 就丢收件箱；答 `DesktopBusy` / `WriteFailed` 把这批放回收件箱头部下一拍再试
- [ ] 渲染：每条汇报一段，整批一个头（有 `dropped` 时注明丢了几条）。`TurnEnded` / `Exited` / `Closed` 带 transcript 增量——从 `reported_cursor` 起到末尾，走 `TranscriptSource::read`（session id 只认 hook，与 `read_transcript` 同一套裁决），角色标 `[user]` / `[assistant]`，字节上界复用 `read_transcript` 的那一档并标 `truncated`，渲染完 `set_reported_cursor`；`transcriptUnsupported` / `sessionUnidentified` 那两档改用 `read_screen` 的尾部若干行。`AwaitingHuman` 带画面尾部原文（编排者要转告用户）。`NotAccepted` 只带任务编号与「写入后 15 秒未见开始处理，请 read-screen 核对后重发」
- [ ] 文案全部走 mt-i18n `orchestrator` 命名空间（10 已建），中英各一份；每段以固定前缀 `[mini-term]` 开头好让编排者认出来；**用户可见面不许出现「乐手」/「musician」**
- [ ] 桌面接线：`crates/mt-app/src/store/ai.rs` 的 `AiEvent::Status` 处理里调 `observe_status`；`dispose_terminal` 里在撤销令牌之前调 `observe_pane_closed`。就这两处
- [ ] 主缝测试（`control.rs` 的 tests，用 `FakeActions` 把写进编排者 pane 的字节抄下来、`pane_liveness` 可控、`FakeTranscripts` 可控）：乐手 Stop → 编排者空闲 → 文字写进编排者 pane 且含 transcript 增量与任务编号；编排者 `ai-working` 时暂存、转 `ai-idle` 后一次投递且多条合并；编排者黄灯暂存；编排者 `idle` 暂存；编排者 pane 没了收件箱丢弃；乐手黄灯 → 汇报含画面原文；无记录 agent → 画面尾部；`NotAccepted` 端到端；`DesktopBusy` 重试不丢；投递用的 `PaneInput` 不带格式尾部
- [ ] 写进编排者的那段文字**不能被 `mt-ai::detect` 误认成 AI 命令、不能被当成打断键**（整块 bracketed paste，tracker 记成一次提交；写一条测试钉住渲染结果不含裸 `\x1b` / `\x03` 单字节）
- [ ] `cargo test --workspace` 全绿（跑 mt-app 单测前关掉 dev 实例）

## 设计要点（给实施方）

- **一切复用**：写编排者走 `send_input`（与 `send` 同一条泵、同一个 `ACTION_TIMEOUT`），读 transcript 走 `transcripts()`，读画面走 `read_screen`，闸的输入走 `pane_liveness`。四条缝都已注入，这一票**不新增 trait 方法**。
- **投递线程一把锁都不许持着做 I/O**：与 `wait` 同款——取事实、放锁、再读文件 / 跳主线程。
- **闸查完到写入之间有窗口**（用户恰好开始打字）：接受，ADR 0004 的「后果」写了。但**不许**在 `ai-working` 时写——那是靠 agent 排队而不是靠闸。
- **编排者的 `MINITERM_*` 身份是 pane**：它里头的 agent 退出再起（同一 pane）令牌仍在，收件箱照留；`can_deliver` 会在它再次 `ai-idle` 时放行。
- 汇报头部信息：乐手 pane 编号、启动器展示名、项目名（记账表里都有）、成因原文、回合耗时（有的话）、任务编号清单。**不回显 prompt 正文**。
- 渲染出的文字体量要有上界（整批不超过 `read_transcript` 单次回执的上限量级）；超了截断并说明——编排者随时能 `read-transcript` 补读。
- 主缝测试里的投递线程节拍要可控：把「1s 自唤醒」做成常量，测试里用显式唤醒（例如 `observe_status` 触发）而不是睡等。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，改完用 `git ls-files --eol` 核对。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。
- 跑 `cargo test -p mt-app --bin mini-term` 前确认没有 dev 实例占着 `target/debug/mini-term.exe`（装机版不占，别杀它）。

## 设计决议（实施方填）

## 留档（实施方填）

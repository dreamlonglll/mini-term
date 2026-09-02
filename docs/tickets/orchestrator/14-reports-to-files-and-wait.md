# 14 — 汇报改为落文件 + `wait` 取件（推翻 12 的终端投递）

**Parent:** ADR 0004（2026-09-02 修订版）

**What to build:** 用户真机一看就否了「整段汇报写穿进编排者终端」：像用户在输入，且上下文线性膨胀。改成：桌面端生成汇报时把正文写成编排者项目目录下的一个文件，**一个字都不写进编排者终端**；`wait` 改为等待**汇报**——有新汇报就返回「哪个会话、什么事、文件在哪」并取走它们，正文由编排者用自己的 Read 工具读。投递泵、投递闸、写穿编排者那一整支删掉。

**Blocked by:** 12（账本与渲染保留复用）、13 文档部分（Skill/help 要再改一遍）

**Status:** todo

- [ ] 汇报文件：每条汇报渲染成 Markdown 写到 `<编排者所在项目>/.mini-term/reports/<编排者 pane 编号>/<NNNN>-<kind>.md`（NNNN 每编排者 4 位递增；kind 用 `turn-ended` / `awaiting-human` / `exited` / `closed` / `not-accepted`）。项目路径取自编排者的授予（`Grant.project_id` → 宿主项目表），取不到时退到应用数据目录 `orchestrator-reports/<pane>/`。写文件走 `mt_core::atomic_write`，目录不存在就建。单文件上限 256 KiB，超了截断并在文末注明
- [ ] 文件内容：抬头改成键值行（`session`、`launcher`、`project`、`kind`、`cause`、`turn`、`tasks`、`at` 等，缺的不出现），空一行后是正文（transcript 增量 / 画面尾部 / 未被接收说明 / 等人处理说明），文案继续走 mt-i18n `orchestrator` 命名空间；删掉批头那两条（`batchHeader` / `batchDropped`）与其它不再用的键，字典重新生成、对账常量更新
- [ ] 账本：`ReportLedger` 的收件箱改成「未取走的汇报」队列，每条带文件路径；`take_batch` 语义不变（取一次即收敛），另加按 `session_pane_id` 过滤的取法；`INBOX_CAP` 溢出照旧丢最旧并计数（`dropped` 随 `wait` 回执带出）
- [ ] 生成即落盘：`observe_status` / `observe_pane_closed` / `tick` 产生汇报时**当场**渲染并写文件（读 transcript 是慢活，仍不许持锁做；`observe_*` 在桌面主线程上被调，渲染与写盘要挪到后台线程——保留 12 的那条常驻线程做「渲染 + 写盘 + tick」即可，只是不再写编排者）。删掉 `can_deliver` / `deliver_pending` / 写穿编排者的 `send_input` 调用与相关测试；`PaneLiveness::awaiting_human` 保留给 `send` 的黄灯闸
- [ ] `wait` 重做：请求 `{targetPaneId?: u32, timeoutMs?}`（`targetPaneId` 变可选，不给 = 名下任一受编排会话）；阻塞到有匹配的未取走汇报即返回并取走，形状 `{"waited": {"outcome": "reports" | "pending", "reports": [{paneId, kind, cause?, taskIds, file, at}], "dropped": n, "waitedMs": n}}`；`pending` 时若给了 `targetPaneId` 附带它此刻的 `status`（给「看不透的 agent」一个信号）；`--timeout 0` 是合法的「只看一眼」。`WAIT_MAX` / `WAIT_DEFAULT` / 读超时那套不变；旧的 `ai-idle` / `attention` / `idle` 三档终态**删掉**（这个功能没发过版，不留兼容）
- [ ] 清理：编排者 pane 关闭（`revoke_pane`）或重新授予（`grant`）时删掉它的 `.mini-term/reports/<pane>/` 整目录（后台删，失败只打日志）；`crates/mt-app/src/orchestrator_skill.rs` 的 `.gitignore` 条目加一条 `.mini-term/reports/`（同一套幂等追加）
- [ ] sidecars/agent-control：`WaitOutcome` 改成新形状（`reports: Vec<ReportView>`，`is_settled` / `needs_human` 按新语义：`needs_human` = 任一条 `kind == "awaiting-human"`），`ControlRequest::wait` 的 `target_pane_id` 变可选；mt-agent-cli 的 `wait` 子命令 `--pane` 变可选；`--help` 的 `reports` 节与 wait notes 重写：汇报在文件里、`wait` 是取件、`pending` 就再等、读文件用你自己的 Read 工具、取走就不再给；`crates/mt-ai/tests/orchestrator_wire.rs` 的 wait 对账改新形状
- [ ] SKILL.md 再改一遍（唯一源头，`cli-location` 标记原样）：「Results are pushed to you」整节换成「Results land as files; `wait` tells you when」——派活 → `wait --timeout 300`（pending 就再 wait；想先干别的就回头再 wait）→ 按回执里的 `file` 用 Read 读 → 处理；五种汇报的处置照旧；样例改成 `wait` 的 JSON 回执 + 一个文件的内容；`orchestrator_skill.rs` 的渲染测试断言随之改
- [ ] 主缝测试（控制面）：回合结束 → 文件存在且内容含 transcript 增量与任务编号 → `wait` 立刻返回并带上该文件路径 → 再 `wait --timeout 0` 是 `pending`（取一次即收敛）；`wait` 阻塞中来了汇报能被唤醒（不靠 250ms 轮询也行，靠也行，但测试不许睡等超过一秒）；按 `--pane` 过滤；黄灯汇报文件含画面原文；未被接收端到端；`dropped` 带出；编排者关闭后目录被删；渲染出的文件不含裸控制字节
- [ ] `cargo test --workspace` + `cargo test --manifest-path sidecars/Cargo.toml --workspace` 全绿；工单 12 与 13 的 Status 与「留档」加一行指到本票

## 设计要点（给实施方）

- **12 的东西能留就留**：`reports.rs` 账本、`transcript_binding`、渲染与 `sanitize_report`、常驻线程、`observe_*` 接线都复用，只是「写穿编排者」换成「写文件 + 入队」。别推倒重来。
- **谁写文件**：mt-ai 的控制面（它已经在读 transcript 文件、已经知道项目路径）。mt-app 只多一条 `.gitignore` 条目。
- **`wait` 的唤醒**：账本入队后戳一下等待中的 `wait` 线程（条件变量或 250ms 轮询皆可，现有 `wait` 就是轮询 `pane_liveness`，照它改成轮询「有没有未取走的汇报」最省）。
- **文件路径回执用绝对路径**（Windows 反斜杠原样），编排者的 Read 工具直接吃。
- **`.mini-term/reports/` 这个目录名**与远程粘贴的 `.mini-term/pasted` 同一个根，是刻意的。
- 术语纪律照旧：用户可见面（`--help`、Skill、汇报文件正文）一律「orchestrated session / 受编排会话」。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，改完用 `git ls-files --eol` 核对。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。不起 GPUI 实例。
- 跑 `cargo test -p mt-app --bin mini-term` 前确认没有 dev 实例占着本 worktree 的 `target/debug/mini-term.exe`（装机版不占，别杀它）。

## 设计决议（实施方填）

## 留档（实施方填）

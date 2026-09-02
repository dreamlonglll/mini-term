# 10 — 派活改造：黄灯拦截 + 任务身份 + 汇报格式尾部（含设置项）

**Parent:** ADR 0004（受编排会话的汇报推送）

**What to build:** `send` 三处改造。① 目标受编排会话停在等人处理（attention）时**拒绝写入**（写入等于代答）；② 每次 `send` 得到一个任务编号，桌面端记下派活时刻与目标当时的状态，回执带回 `taskId` 与 `targetStatus`；③ 桌面端在 prompt 尾部追加一段固定的汇报格式要求（设置可关，默认开）。这张票只动派活这一侧，**不生成任何汇报**——回合追踪在 11，投递在 12。

**Blocked by:** 无（与 11 并行；12 依赖本票的任务账本）

**Status:** todo

- [ ] 目标 `cause` 属 attention（`hook_server::is_attention_cause`）且 pane 活着时 `send` 被拒，错误码 `targetAwaitingHuman`（409），**一个字节都不落到桌面端**（主缝用 FakeActions 断言 `send_input` 未被调用）
- [ ] `send` 成功回执形状 `{sent: {paneId, taskId, bracketedPaste, targetStatus}}`；`taskId` 每编排者单调递增（`t1`、`t2`…，编排者重新授予后从 `t1` 重数）；`targetStatus` 是写入那一刻 `pane_liveness` 的 `status` 原文
- [ ] 任务账本：`ControlPlane` 记 `Task { id, orchestrator_pane_id, target_pane_id, written_at: Instant, target_status_at_write }`，每编排者保留最近 200 条；对外暴露 `pub fn note_task_written`/查询接口给 11、12 用（形状由实施方定，但 12 要能按乐手取「尚未汇报的任务编号」）
- [ ] 格式尾部：`PaneInput` 装配时在正文后追加一个空行 + 尾部文案，**包在 bracketed paste 之内、回车之外**；尾部文案来自 mt-i18n 新命名空间 `orchestrator`（键 `reportFooter`），中英各一份；`ControlPlane::set_report_footer(bool)` 关掉后一个字都不追加
- [ ] 设置项：`AppConfig::orchestrator_report_footer: Option<bool>`（`None` = 开），round-trip 测试照 `orchestrator_session_cap` 那条；设置页「AI」区块并发上限旁边加开关，文案进 `settings.ts`，配置加载/保存时推给控制面（找 `set_session_cap` 的调用点照抄）
- [ ] sidecars/agent-control：`SendReceipt` 增 `task_id: String`、`target_status: String`（都 `#[serde(default)]`，缺省空串——旧桌面端在场时编排者拿到空串去核对，别猜）；`targetAwaitingHuman` 落在退出码 4 那档；mt-agent-cli `--help` 的 send notes 写清三件事（黄灯被拒是规矩不是错误、taskId 的含义、尾部会被追加）
- [ ] `crates/mt-ai/tests/orchestrator_wire.rs`：回执新字段能被 sidecar 解析器读懂；新错误码两侧一致
- [ ] `cargo test -p mt-ai -p mt-config -p mt-app`（mt-app 无 lib 目标，跑单测用 `--bin mini-term`）+ `cargo test --manifest-path sidecars/Cargo.toml --workspace` 全绿；i18n 字典由 `node crates/mt-i18n/tools/gen_from_ts.mjs` 重新生成，`tests/consistency.rs` 的对账常量随之更新

## 设计要点（给实施方）

- **黄灯判据是成因不是状态**：Codex 的 `PermissionRequest` 停在 `ai-working`（`hook_server::map_event_to_status` 的专门一条），只看状态会放过它。`PaneLiveness::cause` 已经带着成因，`send` 里读它即可；判定放在 `resolve_target` 之后、`send_input` 之前。
- **拒绝不是排队**：ADR 0004 定的是「派活即写入，桌面端不排队」，唯一的例外就是黄灯。被拒的正文一个字不缓存，编排者收到 `targetAwaitingHuman` 后自己决定何时重发。错误消息里直接写「tell the user and wait for that session to settle」，与既有 `emptyInput` 那条同一个口气——它是一条裁决，不是入参校验。
- **尾部在控制面装配**：`PaneInput::assemble` 现在只吃正文；加一个带尾部的构造（或给 `assemble` 加参数），两份字节（包裹版 / 裸版）都带尾部。mt-ai 加 `mt-i18n` 依赖（零第三方依赖的叶子 crate，`Cargo.toml` 注释里写明理由），用 `mt_i18n::t("orchestrator", "reportFooter")` 取全局 locale 的文案——mt-app 启动与切语言时已经 `set_locale`，不必再传。
- **尾部文案内容**（中文版意思，英文照译；实施方润色）：
  > 【来自编排者的固定要求】做完后请用以下格式收尾：结果 / 改动的文件 / 做过的验证 / 未完成或存疑的事。遇到不确定的问题，用文字提出并结束本回合等待答复，不要使用交互式提问工具。
- **`targetStatus` 只是事实**，不折成「是不是在排队」的布尔：`ai-working` 意味着 prompt 进了 agent 自己的输入缓冲（Claude / Codex 会排队，Grok 未验），`ai-idle` / `idle` 意味着立刻开跑或写进了裸 shell。`--help` 里把这三档的含义写给编排者。
- **任务账本住在 `TokenRegistry` 旁边、同一把锁**：编排者撤销令牌时它名下的任务一并清掉（与 `sessions` 的 `depart` 同款处置）；乐手关掉时该乐手的任务照留（12 要拿它们做「已关闭」那条汇报的任务清单），等编排者离场再清。
- **回执仍不回显正文**（工单 05 的保密面），`Task` 结构里也**不存正文**。
- **别动 `wait` / `read-*`**，别动 Skill（13 收口）。

## 纪律

- 禁跑 `cargo fmt`。Edit 工具可能把整份文件写成 CRLF，改完用 `git ls-files --eol` 核对，LF 文件保持 LF。
- 不做任何 git 提交 / stash / checkout——由编排会话统一提交。
- 与工单 11 并行在同一个 worktree 里：11 只新增 `crates/mt-ai/src/reports.rs` 与 `lib.rs` 里一行 `mod`，本票**别碰那两处**；cargo 构建锁互相等是正常的，链接期 LNK1104 是杀软扫 exe 的随机撞，重跑一次即可。

## 设计决议（实施方填）

## 留档（实施方填）

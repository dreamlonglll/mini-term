# 03 — 启动乐手：start-session + 范围/上限/套娃裁决

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 编排者能真的起乐手。`start-session` 按启动器 id 在可达项目里启动受编排会话（复用 01 的共享入口；出生礼仪沿用 ADR 0002——不抢焦点、不切项目、一次性提示）；`list-panes` 列出自己乐手及其状态。服务端裁决四条边界（ADR 0003）：分组外项目明确报错；并发上限默认 5——只计存活的 AI 会话中乐手、退出即释放名额、超限返回明确错误（不静默排队）；受编排会话不注入编排令牌（禁套娃）；目标是编排者自己即拒绝（自指禁令）。可见范围铁律：对任何非自启 pane 的读写一律以「不存在」语义拒绝。范围记账（谁 spawn 了谁）由控制服务持有；编排者 pane 重启即新身份，MVP 无收养。

**Blocked by:** 01（共享入口）、02（控制面骨架与令牌）

**Status:** done（b0ad0a6）

- [x] 编排者可在本项目与同分组项目启动乐手并拿到回执；乐手 pane 正常进入 AI 会话
- [x] 出生不抢焦点不切项目，一次性提示照旧（`LaunchPlacement::Background` + 新文案 `orchestratorStartSession`）
- [x] 第 6 个存活乐手的启动请求得到明确「已达上限」错误；乐手退出后名额释放（两条释放路径各一例）
- [x] 乐手 pane 内跑 CLI 被拒（无令牌）；编排者以自己为目标被拒；读写他人 pane 得「不存在」语义
- [x] 启动器不存在 / 项目不可达 / pane 已关的错误语义各自明确
- [x] 主缝测试覆盖以上全部裁决；`cargo test --workspace` 全绿

## 设计决议

**四条裁决各自落在哪**（全部在 `mt_ai::control`，桌面侧只负责「照做」）：

| 裁决 | 落点 |
|------|------|
| 分组外项目 | `ControlPlane::start_session` 的 `reachable_projects` 查找 → `projectUnreachable`。**组外与不存在同码**：区分开来就是一台项目扫描器 |
| 并发上限 | 同函数的 `live_session_count() >= session_cap()` → `sessionLimitReached`（429，不静默排队） |
| 禁套娃 | **类型层面**：`StartSessionSpec` 里没有「要不要授予」这个位；桌面侧 `orchestrator::start_session` 传具名常量 `ORCHESTRATED_GRANT`（恒 `None`），与 `from_launcher` 那条授予路互斥 |
| 自指禁令 | `ControlPlane::resolve_target` 的第一行 → `selfTarget` |

**存活判据**：`alive && status != "idle"`。`alive` 走 `AiBridge::is_pane_live`（500ms 轮询那份活 pane 名册），`status` 走 `AiPerception::status_of`。两条释放名额的路径因此收敛成同一条判据——pane 被关 → `pane_closed` 清名册与状态；agent 自己退出 → hook 的 SessionEnd 把状态落到 `idle`。**每次请求现查**，不靠事件驱动记账：漏收一次事件不该永久占住名额。两样都在后台线程读得到，`pane_liveness` 因此不跳主线程（名额判定一次要问好几个 pane）。

**上限的注入位**：`DEFAULT_SESSION_CAP: usize = 5` + `ControlPlane::set_session_cap()`，内部是一个 `AtomicUsize`（**不加第二把锁**——多一把就多一组与 `registry` 交叉持有的可能）。工单 08 只需在装配处调一次 setter，裁决那一行一个字不动。

**超时 3 秒**（`orchestrator::ACTION_TIMEOUT`）：下界是「建一个 pane 正常几十毫秒」的一个数量级以上；上界是 CLI 的 `READ_TIMEOUT`（5 秒）必须留富余，否则起会话稍慢就变成 CLI 先断线、编排者拿到「够不着」而不是桌面端的明确答复。回执走 `std::sync::mpsc::sync_channel(1)` 而不是 futures oneshot——HTTP 线程是裸线程，没有执行器可 `block_on`。超时/泵不在 → `desktopBusy`，CLI 归进「过会儿再试」那一档退出码（3）。

**阻塞命令另起线程**：hook 那个 HTTP 服务是**单线程 for 循环**，`start-session` 就地阻塞会把排在后面的 hook 上报一起卡住（AI 状态感知是本仓的权威通道，不给编排让路）。于是 `try_handle_control` 对 `blocks_on_desktop()` 的命令：先在 HTTP 线程做掉鉴权（挡住「任意进程都能让我们起线程」），再把已鉴权的活丢一条一次性线程。一条测试用 600ms 的慢动作 + 并发 `/hook` 请求钉住这条。

**记账保留策略**：`revoke_pane` 只作废「该 pane 作为**编排者**」名下的记账（不杀乐手）；乐手自己关闭时那次 `revoke_pane` 刻意**不抹掉**它的记账条目——抹掉了「已关」就退化成「不存在」，而「你起的那个已经关了」是编排者应该知道、且不泄露任何东西的信息。

**新错误码清单**（闭集，CLI 按 code 分档）：

| code | status | 含义 |
|------|--------|------|
| `launcherNotFound` | 404 | 启动器 id 不在名单里 |
| `projectUnreachable` | 403 | 目标项目不可达（组外 / 不存在，**刻意同码**） |
| `remoteProjectUnsupported` | 409 | SSH 远程项目当不了乐手宿主 |
| `sessionLimitReached` | 429 | 已达并发上限 |
| `startFailed` | 500 | 终端没建成 / 命令没交到活着的 PTY |
| `desktopBusy` | 503 | 主线程没在时限内答复（CLI 归「够不着」档，退出码 3） |
| `selfTarget` | 403 | 自指禁令 |
| `paneNotFound` | 404 | 不是自己起的乐手（含压根不存在）——统一「不存在」语义 |
| `paneGone` | 410 | 是自己起的，但那个 pane 已关 |

**线上形状新增**：请求体加 `launcherId` / `projectId` / `targetPaneId`（都 `skip_serializing_if`，`list-*` 的请求体与工单 02 一字不差）；响应加 `PaneView`（`start-session` 回执与 `list-panes` 每条同形）；`ProjectView` 加 `canStartSessions`（远程项目为 `false`，省得编排者白试一次）。`ControlProject` 加 `ssh_connection_id`（**照实投影**，与 `mobile_relay::to_relay_project` 同一份事实；折成 `remote: bool` 会把裁决提前到配置层）。

**`targetPaneId` 提前就位**：05/06/07 的 send/wait/read 要用，两侧同时定稿省得 05 再动一次 CLI 的线上形状；桌面侧暂 `#[allow(dead_code)]`。共享裁决函数 `resolve_target` 本票落地并测试（三条语义各一例），命令留给后续工单。

## 留档（未整改）

- **诞生提示复用 `ToastKind::MobileSession`**：渲染与交互口径完全相同（info 图标 + 点击切项目 + 用 message），只有文案不同。更诚实的做法是把它改名成 `RemoteSession`（移动端与编排者都是「远程发起」），但那是纯重命名、要碰 `mobile_relay.rs` 与 `toast.rs` 的既有代码，合并冲突面大于命名收益。等两条路径都稳定后再一并改名。
- **并发上限有个 TOCTOU 窗口**：同一个编排者并发发两条 `start-session` 时，两条都可能在各自数完名额之后才落地，于是短暂超出上限一个。唯一的收紧办法是持 `registry` 锁跨过 `actions.start_session()`（一条会阻塞好几秒的外部调用），代价远大于收益；而编排者是串行调 CLI 的单进程，实际到不了这个窗口。
- **`list-panes` 会列出已关的乐手**：记账在编排者 pane 生命周期内只增不减（`alive: false`）。一个长命编排者起关几十个乐手会让列表变长。这是有意的（「已关」比「不存在」有用），但工单 04 做「编排者已离场」标识时可以顺带定一条「已关超过 N 条就折叠/淘汰」的口径。
- **`alive` 判据依赖 `AiPerception::pane_closed` 这条路径**：pane 关闭时若哪天不再调它（比如整项目关闭走了别的路），乐手会永远显示为活着并占着名额。当前 `AiBridge::remove_pane` 是唯一注销点，与令牌撤销是同一处，自洽；但这条耦合值得在改动 pane 生命周期时复查。
- **CLI ↔ 真 hook server 的整条 HTTP 往返仍未真机走过**（与工单 02 同一条），主缝/辅缝都是进程内对账。留工单 09 验收。
- **`mt-agent-cli` 与 `miniterm-hook` 的端口发现同形异构**（工单 02 已留档，本票未动）。

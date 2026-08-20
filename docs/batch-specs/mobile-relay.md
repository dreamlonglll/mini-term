# 移动端中转接线 —— 实现规格

> 对应 `docs/gpui-parity-audit.md:57` 缺口 **#22**：
> 「**移动端中转**：mt_relay 实际接线（RelayHost/RelayEvents 实现）+ MobileRelayModal
> （地址/密钥/状态徽章/配对二维码/重置）+ AiLauncherSection CRUD + 5 条事件落点
> （status/pairing-code/start-session/rename-pane/会话结构同步）+ store 补 rename_pane_by_id」。
>
> 权威基线：`src/components/MobileRelayModal.tsx`(249 行)、`src/components/AiLauncherSection.tsx`(212 行)、
> `src/components/RelayStatusBadge.tsx`(37 行)、`src/utils/mobileSessionSync.ts`(104 行)、
> `src/utils/mobileStartSession.ts`(130 行)、`src/App.tsx:334-354`，
> 后端语义已由 `crates/mt-relay/`（`relay.rs` 1340 行 + `host.rs` 131 行 + `mirror.rs`）逐字搬运完成。
>
> 本文所有行号均指向**当前工作树**，实现时请回读源文件核对。
> 文中「原版」= Tauri/React 版，「GPUI」= `crates/` 下的新实现。

---

## 0. 任务边界

### 做什么

1. 在 `crates/mt-app` 里实现 `RelayHost` / `RelayEvents` 两个注入 trait，构造并持有
   `MobileRelayManager`，启动时按配置 `apply` 一次（§1）。
2. 把原版 5 条事件落点搬过来：连接状态 → store、配对码 → 二维码、移动端发起会话 → 建 pane +
   写命令 + 回执、移动端改会话名 → 改 `custom_title`、store 变化 → 结构快照同步（§2）。
3. 实现「移动端」面板（`MobileRelayModal` 等价物）与其中嵌的 AI 启动器 CRUD 段（§3、§4）。
4. 补 store 侧缺口：`rename_pane_by_id`、`mobile_relay` 配置写入、`PaneStatus::as_str`（§5）。
5. 边条加「移动端」入口按钮（§6.4）。

### 不做什么

| 项 | 归属 | 本批处理 |
|---|---|---|
| 协议本身 / `relay-server/` / `mobile/` PWA | 已上现网 v2 | **一个字都不许改**（`crates/mt-relay/src/lib.rs:16-18`） |
| SSH 远程项目的 `ssh_connection_id` 真值 | audit #28（mt-ssh 未进 crates/） | 快照里恒填 `None`，`can_start_session` 因此对远程项目会误判为 true；见 §9 坑 6 |
| 项目分组 `group_path` | audit #13（分组 UI 未做，但**数据在配置里**） | **照做**：`config.project_tree` 已是完整数据，按 §2.5.2 深度优先展开即可 |
| 用量统计 / 会话面板联动 | 无 | 不涉及 |
| toast 悬停暂停 / × 关闭等 | audit「其他细项」 | 发起会话的 toast 走现成 `Notification`，不补功能 |

---

## 1. mt-relay 接线面

### 1.1 依赖与构造

`mt-app/Cargo.toml:23` 已有 `mt-relay.workspace = true`，**当前零 use**（`grep -rn "mt_relay" crates/mt-app/src/` 无命中）。

mt-relay 自身依赖见 `crates/mt-relay/Cargo.toml`：
- `mt-relay-protocol = { path = "../../relay-server/protocol" }` —— 跨工作区 path 依赖，`relay-server` 在
  `Cargo.toml:14` 的 `exclude` 里，但 path 依赖照样能编（已验证：mt-relay 现在就编得过）；
- `tokio-tungstenite` + `rustls`（ring 后端，`relay.rs:447` 运行时 `install_default()`）；
- 自持 tokio 运行时（`relay.rs:76-88` 的 `Spawner`）。

构造（`crates/mt-relay/src/lib.rs:55-61` 的接线概览）：

```rust
let host   = Arc::new(RelayHostImpl::new(...));      // §1.2
let events = Arc::new(RelayEventsImpl::new(tx));     // §1.3
let manager = Arc::new(MobileRelayManager::new(host.clone(), events.clone()));
```

⚠️ `MobileRelayManager::new` 默认**自持一个 2 线程 tokio 运行时**（`relay.rs:147-165`、
`relay.rs:183-192`，首次 `apply` 时惰性创建）。`docs/gpui-migration-progress.md:123` 记着：
「mt-app 若有全局运行时应改用 `with_runtime` 注入，避免进程双线程池」。
**mt-app 目前没有 tokio 运行时**（`crates/mt-app/Cargo.toml` 无 tokio 依赖），
所以本批直接用 `new`，让 mt-relay 自持那两个线程；不要为此给 mt-app 引 tokio。

### 1.2 `RelayHost` —— 入向查询（6 方法）

定义在 `crates/mt-relay/src/host.rs:54-75`。**六个方法都可能在 tokio 工作线程上被调**
（`host.rs:14-15` 明写），因此实现体**一律不许碰 gpui `Entity`**。

| # | 签名 | 语义（原版落点） | GPUI 实现路子 |
|---|---|---|---|
| 1 | `fn launchers(&self) -> Vec<AiLauncher>` (`host.rs:57`) | 当前配置里的启动器名单；每次现取（低频数据）。原版 `mobile_relay.rs:638` 走 `read_config(app)` | **镜像快照**：`Arc<Mutex<Vec<AiLauncher>>>`，主线程在配置变化时刷新（§1.5.2）。配置整块缺失时回落 `mt_config` 的预置两条（`crates/mt-config/src/config.rs:260-275`） |
| 2 | `fn project(&self, project_id: &str) -> Option<RelayProject>` (`host.rs:60`) | 移动端发起会话时校验目标存在且支持；`RelayProject { path, ssh_connection_id }`（`host.rs:46-51`） | 同上，`Arc<Mutex<HashMap<String, RelayProject>>>`，与 `update_sessions` 同一次刷新 |
| 3 | `fn write_pty(&self, pty_id: u32, data: String) -> Result<(), String>` (`host.rs:64`) | **全语义写穿**：等价「本人在桌面对该终端敲了这些字节」（输入跟踪 / AI marker / SSH autofill 解除一个都不能少） | 必须回主线程走 `AppStore::write_to_pane`（`store.rs:618-640`，它内部走 `TerminalPane::write` 而不是裸 PTY 写）。见 §1.5.3 的两种做法与取舍 |
| 4 | `fn hook_session(&self, pty_id: u32) -> Option<HookSessionId>` (`host.rs:68`) | hook 上报过的会话身份；镜像绑定第一层 | **可直接调，无需跳线程**：`ai.perception().hooks().session_of(pty_id)`（`crates/mt-ai/src/hook_server.rs:146`），`HookState` 内部是 `Arc<Mutex<..>>`、`Send + Sync` |
| 5 | `fn ai_session_agent(&self, pty_id: u32) -> Option<String>` (`host.rs:71`) | 输入检测识别到的 agent 名；判断「这个 agent 有没有会话记录」 | 同上：`ai.perception().tracker().ai_session_agent(pty_id)`（`crates/mt-ai/src/tracker.rs:260`） |
| 6 | `fn ai_session_started_at(&self, pty_id: u32) -> Option<SystemTime>` (`host.rs:74`) | 本轮 AI 会话启动时刻（启发式绑定的 mtime 下限） | `ai.perception().tracker().ai_session_started_at(pty_id)`（`tracker.rs:265`） |

即：**4/5/6 直接透传 `AiBridge`（`crates/mt-app/src/ai.rs:46-52`，`#[derive(Clone)]` 且内部全 Arc）；
1/2 走主线程刷新的镜像快照；3 走命令通道回主线程。**

`NoopRelayHost`（`host.rs:102-130`）只给测试用，生产路径注入真实现。

### 1.3 `RelayEvents` —— 出向动作（4 方法）

定义在 `crates/mt-relay/src/host.rs:78-95`。同样在 tokio 线程上被调，**四个全部要跳回主线程**。

| # | 签名 | 语义 | 原 Tauri 事件 |
|---|---|---|---|
| 1 | `fn status_changed(&self, status: MobileRelayStatusPayload)` (`host.rs:80`) | 连接状态变化 | `mobile-relay-status` |
| 2 | `fn pairing_code(&self, code: String)` (`host.rs:83`) | 中转签发的一次性配对码 | `mobile-relay-pairing-code` |
| 3 | `fn rename_pane(&self, payload: RenamePanePayload)` (`host.rs:87`) | 移动端改会话名，标题**已收敛过**（`relay.rs:709-716`：trim + 去控制字符 + 64 字符限长），空串 = 清除自定义名 | `mobile-rename-pane` |
| 4 | `fn start_session(&self, payload: StartSessionPayload)` (`host.rs:94`) | 移动端发起新 AI 会话，**校验已通过**；桌面侧负责建 pane + 写启动命令，**完成后必须回执** | `mobile-start-session` |

载荷结构：
- `MobileRelayStatusPayload`（`relay.rs:91-106`）：`status` 七态串 + `expected_version` / `actual_version`（仅 versionMismatch）+ `paired: Option<bool>`；
- `RenamePanePayload`（`relay.rs:719-725`）：`pane_id` + `title`；
- `StartSessionPayload`（`relay.rs:729-741`）：`request_id` / `project_id` / `launcher_id` / `launcher_name` / `shell_name: Option<String>` / `command`。

### 1.4 `MobileRelayManager` 公开入口（8 个）

`crates/mt-relay/src/lib.rs:40-48` 列了「八个 `#[tauri::command]` 去壳成普通方法」的对照表：

| 方法 | 位置 | 语义 | 何时调 |
|---|---|---|---|
| `apply(self: &Arc<Self>, relay_url: &str, desktop_key: &str)` | `relay.rs:434-459` | 停旧连接；地址非空则起新的重连循环；**空串 = 断开并停用**（`normalize_relay_url` 返回 None → 状态置 `disconnected`） | 启动时（配置里地址非空）+ 面板点「保存并连接」/「断开并清除」 |
| `current_status(&self) -> MobileRelayStatusPayload` | `relay.rs:222-224` | 查当前状态 | 打开面板时取初始值 |
| `request_pairing_code(&self) -> Result<(), String>` | `relay.rs:236-238` | 请求配对码；结果经 `RelayEvents::pairing_code` 回来 | 点「生成配对二维码」 |
| `reset_pairing(&self) -> Result<(), String>` | `relay.rs:241-243` | 吊销移动端全部凭证；结果经状态回调的 `paired` 字段回来 | 点「重置配对」并确认后 |
| `update_sessions(&self, projects: Vec<SyncProject>)` | `relay.rs:252-311` | 喂入项目**全量**状态；内部自行 diff 成增量、维护 pane→路径/PTY 映射、对消失的订阅 pane 发 `PaneClosed` | store 变化去抖后（§2.5） |
| `launchers_changed(&self)` | `relay.rs:246-248` | 重发一次全量快照（不为启动器单开增量） | 启动器增删改保存后 |
| `start_session_result(&self, request_id, ok, pane_id, reason)` | `relay.rs:330-346` | 回执。`ok=true` 带 pane_id；`ok=false` 时 `reason` 缺省填 `SpawnFailed` | 每次 `start_session` 之后**必调**（三硬要求之一） |
| `check_launcher_command(command: &str) -> bool`（自由函数） | `relay.rs:1000-1002` | 命令能否被识别为 AI 会话；内部就是 `mt_ai::is_interactive_ai_command(command.trim())` | 启动器表单里命令输入变化时 |

附带两个构造：`new`（`relay.rs:150`）/ `with_runtime`（`relay.rs:168-176`）。
另外 `can_start_session(path, ssh_connection_id) -> bool`（`relay.rs:493-495`）是公开的，
SSH 远程项目与 WSL UNC 根项目一律 false —— **快照组装时由 mt-relay 自己调**，上层不必重复判定。

### 1.5 线程模型与 GPUI 侧手法

#### 1.5.1 事实

- 连接循环（`relay.rs:511-552`）与镜像轮询（`relay.rs:843-938`）都跑在 mt-relay 自持的 tokio 运行时上；
- `RelayHost` / `RelayEvents` 的**十个方法全部在那两条 tokio 任务里被同步调用**；
- gpui 的 `Entity` 只能在主线程碰（`crates/mt-app/src/ai.rs:10-13` 的同一条红线）。

#### 1.5.2 推荐手法：照抄 `AiBridge` 的 channel 泵

`crates/mt-app/src/ai.rs:30-42` + `main.rs:203-223` 已经把这套跑通了，**逐条照搬即可**：

```rust
// relay.rs（mt-app 侧新模块）
pub enum RelaySignal {
    Status(MobileRelayStatusPayload),
    PairingCode(String),
    RenamePane(RenamePanePayload),
    StartSession(StartSessionPayload),
    /// write_pty 的写穿请求（§1.5.3）
    WritePty { pty_id: u32, data: String },
}

struct RelayEventsImpl { tx: UnboundedSender<RelaySignal> }   // futures::channel::mpsc
impl RelayEvents for RelayEventsImpl {
    fn status_changed(&self, s: MobileRelayStatusPayload) { let _ = self.tx.unbounded_send(RelaySignal::Status(s)); }
    // …四个方法各一行
}
```

主线程侧在 `Workspace::new` 里再起一条泵（与 `_ai_pump` 并列，`main.rs:176`、`main.rs:207-223`）：

```rust
let relay_pump = cx.spawn_in(window, async move |this, cx| {
    while let Some(sig) = rx.next().await { /* this.update_in(cx, ..) 分发 */ }
});
```

- `futures::channel::mpsc::UnboundedSender` 是 `Send + Sync + Clone`，可以放进 `Arc<dyn RelayEvents>`；
- `cx.spawn_in(window, ..)` 拿得到 `Window`，发起会话要建 pane（`AppStore::new_terminal` 需要 `&mut Window`）与弹 toast（`window.push_notification`）都得有它；
- 泵 Task 存进 `Workspace` 的字段（`_relay_pump: Task<()>`），跟 `_ai_pump` 一样靠字段生命周期保活。

#### 1.5.3 `write_pty` 的同步返回怎么办

`RelayHost::write_pty` 签名是**同步返回 `Result`**，而真正的写穿要回主线程。两条路：

**路 A（推荐）：预检 + 乐观 Ok。**
host 侧维护一份 `Arc<Mutex<HashSet<u32>>>` 的「活着的 pty id」镜像（与 §1.5.2 的快照同一次刷新），
`write_pty` 先查这份镜像：不在 → `Err`（映射成 `CommandFailReason::WriteFailed`），在 →
`tx.unbounded_send(RelaySignal::WritePty{..})` 后返回 `Ok(())`。
代价：回执语义从「已写入 PTY」弱化成「已排队写入」。可接受，因为
`relay.rs:796-797` 的注释本来就写着「回执仅表示『已写入 PTY』，AI 真正接收以镜像回流为准」，
而 `handle_mobile_command`（`relay.rs:798-839`）在调 `write_pty` **之前**已经用
mt-relay 自己的 `pane_ptys` 映射挡掉了 `PaneNotFound` 那一档。

**路 B：oneshot 回执 + 超时阻塞。**
发信号时带一个 `std::sync::mpsc::SyncSender<bool>`，在 tokio 线程上 `recv_timeout(2s)`。
语义精确，但会阻塞那个 2 线程运行时的一个 worker；主线程若正卡在长帧上就要等满超时。
**只有在真出现「回执说成功但命令没进去」的投诉时才换这条路。**

无论哪条，主线程侧的落点都是：
```rust
store.update(cx, |store, cx| store.write_to_pane(&project_id, &pane_id, &data, cx))
```
需要 pty_id → (project_id, pane_id) 的反查（store 侧现有 `SplitNode::panes()`，`tree.rs:235`；
建议在 store 上加一个 `pane_of_pty(pty_id) -> Option<(String, String)>` 的小方法，§5.4）。

### 1.6 三条硬要求（`docs/gpui-migration-progress.md:36` 逐字）

1. **`write_pty` 必须是全语义写穿口** —— 走 `AppStore::write_to_pane` / `TerminalPane::write`，
   不许直接摸 `mt_pty`；否则 pane 进不了 AI 会话状态，手机上永远等不到会话出现。
2. **`start_session` 后必回执 `start_session_result`** —— 任何一条 early return 都要先回失败回执，
   否则手机侧转圈到 15s 超时（原版 `mobileStartSession.ts:42-51` 的 `reportResult` 在**每一条**失败路径上都调了）。
3. **启动器命令文本绝不进快照**（ADR 0002） —— `send_snapshot`（`relay.rs:415-430`）只取 `id` + `name`
   组 `MobileLauncher`，这条已经在 mt-relay 里落死了；**上层要做的是别自己另开一条把 command 送出去的路**。

---

## 2. 原版事件落点（5 条）+ 桌面侧三条链路

### 2.1 `mobile-relay-status` → 连接状态

原版：`src/App.tsx:334-337`
```ts
useTauriEvent<MobileRelayStatusPayload>('mobile-relay-status', (payload) => {
  useAppStore.getState().setMobileRelayStatus(payload);
});
```
store 侧：`src/store.ts:702-703`（字段声明）、`src/store.ts:752-753`（`setMobileRelayStatus`）。
消费方只有面板里的徽章（`MobileRelayModal.tsx:33`、`:184`）与配对状态行（`:112`、`:197-201`）。

**GPUI**：`RelaySignal::Status` → 主线程 → 写进一个运行时状态（**不落盘**）。
放哪：建议放 `AppStore` 上加 `mobile_relay_status: Option<MobileRelayStatusPayload>` 字段 +
`set_mobile_relay_status(..., cx)`（内部 `cx.notify()`），与 `src/store.ts` 的位置一致；
面板视图 `cx.observe(&store)` 即可跟着重画。

状态串七态（`relay.rs:93-97`、`src/types.ts:112-121`）：
`disconnected | connecting | connected | reconnecting | versionMismatch | authFailed | keyNotConfigured`。

### 2.2 `mobile-relay-pairing-code` → 二维码

原版：`MobileRelayModal.tsx:88-93`
```ts
useTauriEvent<{ code: string }>('mobile-relay-pairing-code', (payload) => {
  const pairUrl = `${relayHttpBase(relayUrl)}/#pair=${payload.code}`;
  QRCode.toDataURL(pairUrl, { width: 260, margin: 1 }).then(setQrDataUrl).catch(() => setQrDataUrl(null));
});
```
`relayHttpBase`（`MobileRelayModal.tsx:19-26`）—— **逐字照搬**：
```
去尾部斜杠 → wss:// → https:// ；ws:// → http:// ；已是 http(s):// 原样；其余前缀补 https://
```

**GPUI**：`RelaySignal::PairingCode(code)` → 主线程 → 用**面板打开时的 `relay_url`**（不是输入框里的草稿值）
拼 pair URL → 生成 QR → 写进面板状态 Entity。渲染方案见 §3.3。

⚠️ 事件可能在面板关着时到达（原版靠 hook 只在 modal mount 时注册；GPUI 的泵是全局的）。
**面板关着时直接丢弃**：原版关闭面板会 `setQrDataUrl(null)`（`MobileRelayModal.tsx:56-61`），
留着的旧码可能已被后续操作作废。

### 2.3 `mobile-start-session` → 建 pane + 写命令 + 回执

原版：`src/App.tsx:344-349` → `src/utils/mobileStartSession.ts:54-130`。**逐步照抄**：

| 步 | 原版行 | 动作 | 失败回执 |
|---|---|---|---|
| 1 | `mobileStartSession.ts:59-63` | 按 `projectId` 找项目 | `projectNotFound` |
| 2 | `:66-69` | 解析 shell：`shellName` 指定的 → `defaultShell` → 列表首项（**绑定的 shell 被删掉时退回默认，总比不开好**） | — |
| 3 | `:71-74` | 一个 shell 都没配 | `spawnFailed` |
| 4 | `:76-82` | 建 PTY | `spawnFailed` |
| 5 | `:84-93` | 建 pane：`customTitle = launcherName`（回到电脑前一眼看出这个标签是什么），`shellName` 存**实际** shell（布局恢复靠它查 availableShells），`status = 'idle'` | — |
| 6 | `:95-98` | **先建终端实例再写命令**：pty-output 只写进已缓存的实例，cache miss 直接丢弃 —— 不提前建，AI 起来那一整段输出全丢 | — |
| 7 | `:100-110` | 挂进布局树**最左侧 leaf 的 tab 栏末尾**（`appendPaneToFirstLeaf`，`:32-40`），**不动 activePaneId、不切项目**；项目还没布局时新建根 leaf | — |
| 8 | `:112-118` | `write_pty(ptyId, `${command}\r`)` | `spawnFailed`（**pane 保留**，用户回桌面能看到它卡在哪） |
| 9 | `:120-127` | 桌面端 toast：kind `mobile-session`，文案 `t('app.mobileStartSession', { launcher })`。注释写着「凭证被盗时这是唯一的审计迹象，所以即便不切过去也要弹」 | — |
| 10 | `:129` | `reportResult(requestId, true, paneId)` | — |

**GPUI 映射**：
- 步 4~7 现成的最近路径是 `AppStore::new_terminal(project_id, Some(shell), anchor, window, cx)`（`store.rs:372-397`），
  但它的挂载语义是「加进**锚点 pane 所在**叶子并**激活 + 抢焦点**」（`store.rs:385-395`），
  与原版「最左侧 leaf 末尾、不激活、不抢焦点」**不一致**。
  ⇒ 需要一个新方法 `AppStore::append_pane_background(project_id, shell, custom_title, window, cx) -> Option<String>`：
  内部复用 `spawn_pane`（`store.rs:800-813`）拿 `PaneState`，设 `custom_title`，
  然后**只**做 `layout.append_pane(None, pane)` 语义的最左叶追加，不调 `focus_pane`、不调 `activate_pane`。
  （`SplitNode::append_pane(anchor, pane)` 见 `tree.rs`；anchor 传 `None` 时的落点要核对是否等价「最左侧 leaf」，
  不等价就自己写一个 `append_to_first_leaf`。）
- 步 6 在 GPUI 里是**自动满足**的：`start_pty`（`store.rs:822-877`）建 PTY 的同时就 `cx.new(TerminalPane::new(..))`
  并插进 `self.terminals`，不存在「实例还没建」的窗口期。原版那条注释可以在新方法里留一句说明为什么不用做。
- 步 8 走 `write_to_pane`（`store.rs:618-640`）。
- 步 9 走 `Workspace::deliver_alert` 同款的 `window.push_notification(Notification::success(..))`（`main.rs:288-308`），
  文案 key `app.mobileStartSession`（`crates/mt-i18n/src/dict.rs:40` / `:84`，带 `{launcher}` 插值，用 `tr!("app","mobileStartSession", launcher = name)`）。
  原版 toast 的 kind 是 `mobile-session`，GPUI 侧没有 kind 体系，用 `Notification::info`/`success` 即可，
  但**要给它一个独立的去重键类型**（照 `main.rs:140-141` 的 `CompletionToast`/`AttentionToast` 加一个 `MobileSessionToast`），
  否则会和 AI 完成 toast 互相顶掉。
- 步 10 与所有失败分支：`manager.start_session_result(request_id, ok, pane_id, reason)`。

`StartSessionFailReason` 五值（`relay-server/protocol/src/lib.rs:71-82`）：
`DesktopOffline`（中转侧用，桌面端不发）/ `ProjectNotFound` / `LauncherNotFound` / `NotSupported` / `SpawnFailed`。
前三者在 mt-relay 里已经判过了（`relay.rs:747-778`），到 `RelayEvents::start_session` 时只剩 `SpawnFailed` 会用到 ——
**但 `ProjectNotFound` 仍要保留**：从校验到执行之间用户可能刚好把项目移除了。

### 2.4 `mobile-rename-pane` → 改 pane 标题

原版：`src/App.tsx:351-354`
```ts
// 不回执——改完的新名字会随结构增量推回手机，那既是反馈也是真相。
useAppStore.getState().renamePaneById(payload.paneId, payload.title);
```
store：`src/store.ts:1163-1180`，语义见 §5.1。

**GPUI**：`RelaySignal::RenamePane` → `store.rename_pane_by_id(&pane_id, &title, cx)`。
**不回执**（这条要写进注释，否则实现者会习惯性地补一个）。

### 2.5 会话结构同步 `mobile_relay_update_sessions`

原版：`src/App.tsx:339-342` 挂 `initMobileSessionSync()`，实现在 `src/utils/mobileSessionSync.ts`。

#### 2.5.1 触发时机与去抖

`mobileSessionSync.ts:96-104`：`useAppStore.subscribe(...)` 全量订阅 → **150ms 去抖** → `syncNow()`；
挂载时先同步一次。`syncNow()`（`:84-93`）还做了一层**内容去重**：`JSON.stringify` 与上次一致就不发；
`invoke` 失败时把 `lastSentJson` 清空（下次状态变化会重试）。

**GPUI**：在 Workspace（或专门的 `relay.rs` 桥模块）里 `cx.observe(&store, ..)` +
`cx.spawn(async { background_executor().timer(150ms).await; .. })` 的代号去抖
（照 `AppStore::save_config_soon`，`store.rs:1459-1478`，用 `generation` 计数防旧任务晚到）。
内容去重改成比较**上一次组好的 `Vec<SyncProject>`**（`SyncProject` 没有 `PartialEq`，
要么给它加，要么比 `serde_json::to_string` —— 后者与原版口径完全一致，推荐）。

#### 2.5.2 快照组装规则（`mobileSessionSync.ts:1-13` 的可见性规则，逐条照抄）

- **项目：上报全集**（不是「只有活跃会话的项目」）—— 手机的发起弹层要能选到没有会话的项目；
- **顺序**：项目树的**深度优先序**（不是 `config.projects` 的存储序），见 `src/utils/projectTree.ts:296-321`
  的 `getProjectsWithGroupPath`：
  - 递归 `config.projectTree`，遇 `Group` 就把 `group.name` 压进 `groupPath` 继续下钻；
  - 已见过的项目 id 跳过（`seen` 去重）；
  - **不在树里的项目**（异常配置兜底）按 `config.projects` 顺序追加到顶层，`groupPath = []`。
  - GPUI 侧数据结构：`mt_config::ProjectTreeItem`（`crates/mt-config/src/config.rs:38-53`，`untagged` 枚举：
    `ProjectId(String)` | `Group(ProjectGroup{id,name,collapsed,children})`），`AppConfig::project_tree: Option<Vec<..>>`。
  - **桌面端折叠态不下发**（`mobileSessionSync.ts:7`）。
- **pane：只有 AI 会话中的进快照** —— `status ∈ {ai-working, ai-idle}`，
  外加「**曾是 AI 会话且现处 error 态**」的 pane（`:41-42` `aiPaneIds` 集合 + `:59-61`）。裸 shell 一律不出现。
  - `aiPaneIds` 是**跨调用保留的状态**：每轮重算出 `nextAiPaneIds` 再整体替换（`:49`、`:62`、`:80`）。
    GPUI 侧要把它挂在桥模块的字段上，别做成局部变量。
- **pane 字段**（`:63-68`）：`paneId` = pane.id；`title` = `customTitle ?? shellName`；`status` 原样串；`ptyId`（可缺省）。
- **project 字段**（`:70-77`）：`projectId` / `name` / `path`（**镜像绑定用，不转发给移动端**）/
  `sshConnectionId`（后端据此判 `canStartSession`）/ `groupPath` / `panes`。

`SyncProject` / `SyncPane` 的 Rust 定义在 `crates/mt-relay/src/relay.rs:38-62`
（注意它们**只 derive `Deserialize`**，上层要构造得自己填字段 —— 字段都是 `pub`，直接结构体字面量即可）。

`PaneStatus` 目前只有 `from_str`（`crates/mt-app/src/tree.rs:55-63`），**缺 `as_str`**，见 §5.3。

### 2.6 桌面侧三条链路（原版 Rust，已由 mt-relay 搬完，接线时当参考读）

| 链路 | mt-relay 位置 | 要点 |
|---|---|---|
| **项目快照 / 增量** | `relay.rs:252-311`（`update_sessions`）+ `relay.rs:941-967`（`diff_sessions`）+ `relay.rs:415-430`（`send_snapshot`） | 增量口径是**整项目 upsert**（内容变了就整条重发）+ 项目移除列表；无变化返回 `None` 不发。握手成功后（`relay.rs:631`）与收到 `SessionsSnapshotRequest`（`relay.rs:671`）时发全量 |
| **写穿 PTY** | `relay.rs:798-839`（`handle_mobile_command`） | `pane_ptys` 查不到 → `PaneNotFound`；写 `format!("{text}\r")` 一次写入 = 敲入内容并回车；成功后 `record_mobile_cmd`（每 pane 上限 20 条，`relay.rs:370-377`），供镜像回流时把匹配的 user 消息改标 `"mobile"`（`relay.rs:381-395`） |
| **start_session 回执** | `relay.rs:314-346` | `send_start_receipt` 组 `StartSessionReceipt{request_id, ok, pane_id, reason}`；`start_session_result` 在 `ok=false` 时把 `reason` 缺省成 `SpawnFailed` |

原版桌面侧取状态的三处（`src-tauri/src/mobile_relay.rs:772-798`）现在全部收敛到 `RelayHost` 的 4/5/6 三个方法上，
GPUI 侧不必再看那段。

---

## 3. MobileRelayModal 逐控件规格

源：`src/components/MobileRelayModal.tsx`（249 行）。

### 3.1 外壳

| 项 | 原版 | 行 | GPUI |
|---|---|---|---|
| 标题 | `t('mobileRelay.modal.title')` | `:118` | `Dialog::title` |
| 面板尺寸 | `w-[440px] max-h-[76vh]` | `:119` | `.w(px(440.))`，高度靠内容滚动 |
| 遮罩点击关闭 | **`closeOnOverlay={false}`**（面板内有未保存的地址/密钥输入） | `:120-121` | `Dialog::overlay_closable(false)`；Esc 仍可退 |
| 正文容器 | `flex-1 overflow-y-auto px-5 py-4 space-y-4` | `:124` | `.px(px(20.)).py(px(16.))` + 纵向 gap 16 + 可滚动 |
| 打开时 | `invoke('mobile_relay_status')` 兜底刷一次状态 | `:56-65` | 直接 `manager.current_status()` 写进 store |
| 关闭时 | 清 `qrDataUrl` / `qrRequested` | `:57-61` | 面板状态 Entity 随对话框销毁即可，但**全局 pairing_code 泵要知道面板关了**（§2.2） |
| 防叠开 | 原版没有（audit 记的缺口） | — | 走 `prompt::open_guarded(kind::MOBILE_RELAY, ..)`，新增一个 `overlay::kind` 常量（`crates/mt-app/src/overlay.rs:50-69`） |

### 3.2 控件表（15 条，自上而下）

| # | 控件 | 原版行 | 规格 |
|---|---|---|---|
| 1 | 说明段 | `:125` | `t('mobileRelay.intro')`，`text-sm text-[--text-muted] leading-relaxed` |
| 2 | 「中转服务器地址」标签 | `:128-131` | `text-base text-[--text-muted] uppercase tracking-[0.1em] mb-2` —— 对应 `ui::section_title`（`ui.rs:380`，需核对是否同款 uppercase/字距） |
| 3 | 地址输入框 | `:132-140` | `type=text`，`spellCheck=false`，占位 `mobileRelay.urlPlaceholder`；**Enter = 保存并连接**（`:136`）；focus 边框转 accent |
| 4 | 「桌面端接入密钥」标签 | `:143-145` | 同 2，`mt-3 mb-2` |
| 5 | 密钥输入框 | `:146-155` | **`type=password`** + `autoComplete=off` + `spellCheck=false`；Enter 同样触发保存（`:150`）。GPUI：`InputState` 有无 masked 模式要核对；没有就退而求其次用普通输入并在注释里记档（**不要明文常驻**是原版的意图） |
| 6 | 密钥说明 | `:156-158` | `t('mobileRelay.keyHint')`，`text-sm text-[--text-muted] mt-1` |
| 7 | 「保存并连接」按钮 | `:161-167` | accent 实心系（`bg-[--accent-muted] text-[--accent] border-[--accent]`），**`disabled={!url.trim()}`**；动作 = `applyRelaySettings(url, key)`（§3.6） |
| 8 | 「断开并清除」按钮 | `:168-174` | ghost 系；**`disabled={!url.trim() && !relayUrl}`**；动作 = `setUrl(''); applyRelaySettings('', key)` —— **保留密钥与启动器**（`:67-68` 注释：它们与「这次是否建连」无关，别让用户重填） |
| 9 | AI 启动器段 | `:179` | 嵌 `<AiLauncherSection />`，**与是否连上中转无关，始终可编辑**（`:178` 注释） |
| 10 | 连接状态行 | `:182-185` | 一行 `justify-between`：左 `t('mobileRelay.statusLabel')`，右 `RelayStatusBadge`（§3.5）。容器 `px-3 py-2.5 rounded-[--radius-md] bg-[--bg-base] border-[--border-subtle]` |
| 11 | 未配置提示 | `:187-190` | **`relayUrl` 为空时**只显示 `t('mobileRelay.modal.notConfigured')`，下面 11~15 全部不渲染 |
| 12 | 配对状态行 | `:194-203` | 同 10 的容器；右侧三态文案：`paired===true` → `modal.paired`；`===false` → `modal.notPaired`；`undefined` → `modal.pairedUnknown` |
| 13 | 二维码区 | `:206-223` | **仅 `status === 'connected'` 时**渲染（`:42` `connected`）；否则显示 `t('mobileRelay.modal.needConnected')`（`:242`）。有码 → 图 + `modal.qrHint`；已请求未回 → `modal.qrWaiting`；都没有 → 什么都不画 |
| 14 | 生成/重新生成按钮 | `:225-230` | accent 系；文案随 `qrDataUrl` 在 `modal.regenerateQr` / `modal.generateQr` 之间切；动作 = `setQrRequested(true); setQrDataUrl(null); invoke('mobile_relay_request_pairing_code')`，**失败则把 `qrRequested` 撤回**（`:95-99`） |
| 15 | 重置配对按钮 | `:231-238` | **仅 `paired === true` 时出现**；danger 系（`text-[--color-error]`，hover 边框转 error）；动作见 §3.4 |

### 3.3 二维码渲染（原版用什么、GPUI 怎么办）

**原版**：`import QRCode from 'qrcode'`（`MobileRelayModal.tsx:4`，npm 包 `qrcode`），
`QRCode.toDataURL(pairUrl, { width: 260, margin: 1 })` → data URL → `<img>`
（`:208-216`：`width/height=260`，`rounded-[--radius-md] border-[--border-subtle] bg-white p-1`）。
⚠️ `margin: 1` 是**模块单位**的静区，不是 4（`qrcode` 包的默认是 4）—— 照抄 1。
纠错级别未指定 = `qrcode` 包默认 **'M'**。

**GPUI 方案 A（推荐）**：加一个纯 Rust 的 QR 编码库，自己画：
```toml
# crates/mt-app/Cargo.toml
qrcode = { version = "0.14", default-features = false }   # ⚠️ 默认 feature 含 image，必须关
```
- `QrCode::with_error_correction_level(pair_url.as_bytes(), EcLevel::M)` → `code.width()` + `code.to_colors()`；
- 渲染走 `gpui::canvas(prepaint, paint)`（`crates/mt-app/src/terminal_area.rs:650`、`:775` 有现成用法），
  在 paint 闭包里 `window.paint_quad(fill(rect, color))`（`crates/mt-ui/src/terminal/element.rs:1445` 等处的同款）；
- 布局：外框 260×260，白底（**固定白色，不跟主题** —— 相机识别需要高对比），
  静区 1 模块，`module_px = floor(260 / (width + 2))`，实际绘制尺寸 `module_px * (width + 2)` 居中。
- **不要**用 `mt_ui::icons::VectorIcon`：它的 `new(shapes: &'static [Shape], ..)`（`crates/mt-ui/src/icons/vector.rs:227`）
  只吃 `'static` 形状表，QR 是运行时数据，塞不进去。

**GPUI 方案 B（兜底，若不愿引依赖）**：不画码，改成「配对链接文本 + 复制按钮」
（`gpui_component::clipboard::Clipboard` 现成，`~/.cargo/.../gpui-component-0.5.1/src/clipboard.rs:15`），
`modal.qrHint` 文案里「扫码」那半句会对不上，需要在 TS 源头补一条降级文案。**功能可用但是降级，须记档。**

### 3.4 重置配对流程

`MobileRelayModal.tsx:101-110`：
1. `ask(t('mobileRelay.modal.resetConfirm'), { title: t('mobileRelay.modal.resetPairing'), kind: 'warning' })`
   —— Tauri 的系统确认框；
2. 用户取消 → 直接 return；
3. 确认 → 清 `qrDataUrl` / `qrRequested` → `invoke('mobile_relay_reset_pairing')`（失败静默）。

**GPUI**：走现成的 `prompt::Confirm::new(title, message).open(on_ok, window, cx)`（`crates/mt-app/src/prompt.rs:154-207`）。
⚠️ 确认框与移动端面板是**不同种类**的覆盖物，`open_guarded` 允许叠开（`overlay.rs:203-210` 有 pin 测试），
不必先关面板。确认后调 `manager.reset_pairing()`（`Result` 忽略，与原版 `.catch(() => {})` 同）。

### 3.5 `RelayStatusBadge`

源：`src/components/RelayStatusBadge.tsx`（37 行）。

- 颜色表（`:4-13`）：
  | status | 颜色 token |
  |---|---|
  | `connected` | `--color-success`（`ui::color_success()`） |
  | `connecting` / `reconnecting` | `--color-ai-working`（`ui::color_ai_working()`） |
  | `disconnected` | `--text-muted`（`ui::text_muted()`） |
  | `versionMismatch` / `authFailed` / `keyNotConfigured` | `--color-error`（`ui::color_error()`）—— 三种「配置问题」终态，**已停止重连**，红点提示要人动手 |
  | 认不出的串 | 回落 `--text-muted` |
- 文案（`:19-24`）：`versionMismatch` 走带参插值
  `t('mobileRelay.status.versionMismatch', { expected, actual })`（缺省 `'?'`），
  其余直接 `t('mobileRelay.status.' + status)`；
  GPUI：`tr!("mobileRelay", "status.versionMismatch", expected = e, actual = a)`（`crates/mt-i18n/src/lib.rs:421-430`）。
  **动态拼 key 的那一支要写成 match，不能真拼字符串** —— `t()` 的 `debug_assert` 与 `i18n.rs` 的
  `USED_KEYS` 表都要求 key 是字面量（§7）。
- 形状（`:26-36`）：`w-2 h-2 rounded-full`（8px 圆点）+ 8px gap + 文字；
  `connecting`/`reconnecting` 带 `animate-blink`。
  ⚠️ 用户机器 `prefers-reduced-motion: reduce`（见记忆库），闪烁在他机器上原本就看不见 ——
  GPUI 侧可以不做闪烁动画，但**颜色必须对**。
- 容器：`text-base text-[--text-secondary] max-w-[70%] text-right`（长文案要能换行不撑破面板）。

### 3.6 `applyRelaySettings` 的完整顺序（`MobileRelayModal.tsx:69-85`）

```
trim(url) / trim(key)
→ 清 QR 状态（地址变了旧配对二维码即作废）
→ 从 store 现取 config（不是闭包里那份，防陈旧）
→ mobileRelay = withMobileRelayDefaults(cfg.mobileRelay, { relayUrl, desktopKey })
→ setConfig(newConfig)
→ await saveConfigToDisk(newConfig)
→ await invoke('mobile_relay_apply', { relayUrl, desktopKey })
```
`withMobileRelayDefaults`（`src/utils/mobileRelayConfig.ts:13-18`）：
`{ relayUrl:'', desktopKey:'', launchers:[], ...current, ...patch }` ——
**没碰的字段不许写丢**；`current` 为空时 `launchers` 取空列表**而不是**塞回预置两条
（凭空补预置会跟后端「用户删光是有意结果」的迁移规则打架，`:8-10` 注释）。

**GPUI**：`AppStore::set_mobile_relay_endpoint(url, key, cx)`（§5.2）内部只改这两个字段、
其余保持；然后 `save_config_now()`（此处要**立即**写盘，原版是 await 落盘后才 apply）→ `manager.apply(&url, &key)`。

---

## 4. AiLauncherSection 逐控件规格

源：`src/components/AiLauncherSection.tsx`（212 行）。模块头注释（`:10-21`）说明了 ADR 0002 的边界：
**这里不提供任何让移动端自拟命令的入口**。

### 4.1 状态

- `launchers = config.mobileRelay?.launchers ?? []`（`:37`）；
- `draft: DraftState | null`（`:39`）—— `{ id, name, shell, command }`，`id === ''` 表示新增（`:23-31`）；
- `commandWarning: boolean`（`:40`）。

### 4.2 控件表（15 条）

| # | 控件 | 原版行 | 规格 |
|---|---|---|---|
| 1 | 段标题 | `:100-102` | `t('mobileRelay.launchers.title')`，同 §3.2 的标签样式 |
| 2 | 段说明 | `:103-105` | `t('mobileRelay.launchers.intro')` |
| 3 | 空列表警告 | `:107-111` | **`launchers.length === 0 && !draft`** 时显示 `t('mobileRelay.launchers.empty')`，**红字**（`text-[--color-error]`） |
| 4 | 行·品牌图标 | `:120-122` | `<BrandIcon vendor={inferVendor({ command })} size={16} />`。GPUI：`mt_ui::icons::AiVendor::infer(None, Some(&launcher.command))`（`crates/mt-ui/src/icons/brand.rs:133`）+ `BrandIcon::new(vendor).size(px(16.))`（`brand.rs:584`、`:593`），识别不出回落 Bot（`BrandIcon::new(None)`） |
| 5 | 行·名称 | `:124` | `text-base text-[--text-primary] truncate` |
| 6 | 行·副行 | `:125-127` | **`shell ? "{shell} › {command}" : command`**（U+203A 单右角引号），`text-sm text-[--text-muted] truncate font-mono` |
| 7 | 行·编辑按钮 | `:129-141` | 文案 `launchers.edit`；点击把该条填进 draft（`shell: launcher.shell ?? ''`）；hover 转 accent |
| 8 | 行·删除按钮 | `:142-147` | 文案 `launchers.delete`；**无二次确认**（原版就是直接删）；hover 转 error；动作 = `persist(launchers.filter(l => l.id !== id))` |
| 9 | 行容器 | `:115-118` | `flex items-center gap-2 px-3 py-2 rounded-[--radius-sm] bg-[--bg-base] border-[--border-subtle]`；列表纵向 `space-y-1.5`（6px） |
| 10 | 草稿·名称输入 | `:154-160` | 占位 `launchers.namePlaceholder` |
| 11 | 草稿·shell 下拉 | `:161-172` | 第一项 `value=""` 文案 `launchers.defaultShell`；其余为 `config.availableShells` 的 `name`。GPUI：`gpui_component::select::Select`（`select.rs:358`/`:920-989`）是候选，但**mt-app 尚无使用先例**；保险做法是用已落地的自建菜单 `crates/mt-app/src/menu.rs` 做一个 picker 按钮（点开列出「使用默认 shell」+ 各 shell 名，勾选态走「✓ 」文本方案），与 N 批的菜单风格一致 |
| 12 | 草稿·命令输入 | `:173-180` | 占位 `launchers.commandPlaceholder`，`spellCheck=false`，**`font-mono`** |
| 13 | 命令识别警告 | `:181-185` | `commandWarning` 为真时显示 `t('mobileRelay.launchers.commandWarning')`，颜色 **`--color-ai-working`（黄）不是红** —— 它不阻塞保存 |
| 14 | 草稿·保存按钮 | `:186-193` | accent 系；**`disabled={!name.trim() \|\| !command.trim()}`**；动作见 §4.4 |
| 15 | 草稿·取消 / 新增按钮 | `:194-209` | 取消 = `setDraft(null)`；无 draft 时底部显示 `+ {t('mobileRelay.launchers.add')}`，点击 `setDraft({...EMPTY_DRAFT})` |

**编辑中的行让位**：原版把草稿表单渲染在列表**下方**（`:152`），编辑时那一行仍然显示。
（对照 `crates/mt-app/src/modal.rs:133-137` 的终端配置是「编辑中的行让位给表单」—— **两者不同，别照搬 modal.rs**。）

### 4.3 命令识别警告的判定逻辑（`:42-61`）

```ts
useEffect(() => {
  const command = draft?.command.trim() ?? '';
  if (!command) { setCommandWarning(false); return; }        // 空命令不提示
  invoke<boolean>('mobile_relay_check_launcher_command', { command })
    .then(recognized => setCommandWarning(!recognized))
    .catch(() => setCommandWarning(false));                  // 后端不可用时不提示，别拿假警告吓人
}, [draft?.command]);
```
- **无去抖**，每次输入变化查一次（后端就是个纯函数，便宜）；
- `cancelled` 标志防旧请求晚到覆盖新结果（`:47`、`:58-60`）。

**GPUI**：`mt_relay::check_launcher_command(&command)` 是**同步纯函数**（`relay.rs:1000-1002`），
直接在渲染/输入回调里调即可，`cancelled` 那套竞态处理整个不需要。

判定口径（`relay.rs:1200-1214` 的 pin 测试）：`claude` / `codex` / `grok` / `claude --dangerously-skip-permissions`
/ `grok --resume` 为真；`npm test` / `claude -p 'hi'` / `codex --version` / 空串为假。
**这个口径必须与 PTY 输入检测同源**（两处漂移就会出现「面板说没问题、手机上却永远等不到 AI 会话」）。

### 4.4 保存与持久化

`saveDraft`（`:75-91`）：
```
name = trim(draft.name); command = trim(draft.command)
if (!name || !command) return                  // 静默 return，按钮本来就 disabled
entry = { id: draft.id || genId(), name, command, ...(draft.shell ? { shell } : {}) }
                                               // ⚠️ shell 为空串时字段整个不写，不是写 ""
next = draft.id ? 替换同 id 那条 : 追加到末尾
setDraft(null)                                 // 先收表单再落盘
await persist(next)
```
`persist`（`:64-73`）：
```
cfg = store 现取（不是闭包里那份）
newConfig = { ...cfg, mobileRelay: withMobileRelayDefaults(cfg.mobileRelay, { launchers: next }) }
setConfig(newConfig)
await saveConfigToDisk(newConfig).catch(()=>{})
await invoke('mobile_relay_launchers_changed').catch(()=>{})   // 手机侧弹层立即看到新名单
```

**GPUI**：`AppStore::set_launchers(next, cx)`（§5.2）→ `save_config_now()` → `manager.launchers_changed()`。
`genId()`：mt-app 有 `crate::tree::gen_id(prefix)`（`tree.rs`），
原版 `genId()` 生成的是无前缀随机串 —— **id 只在本机配置内使用，格式不影响协议**，用哪个都行，
但 `mt_config::AiLauncher` 的预置条目 id 是 `"claude"` / `"codex"`（`config.rs:262-273`），别与之冲突。

---

## 5. store 缺失 action 与补丁（5 条）

### 5.1 `rename_pane_by_id`（audit 明列的那条）

原版 `src/store.ts:1163-1180`，声明在 `:656`。**逐条语义**：

```ts
renamePaneById: (paneId, title) => set((state) => {
  const nextTitle = title || undefined;          // 空串 = 清掉自定义名，回落 shell 名
  for (const [pid, ps] of newStates) {           // 遍历所有项目
    if (!ps.layout) continue;
    const layout = updatePaneById(ps.layout, paneId, pane =>
      pane.customTitle === nextTitle ? pane : { ...pane, customTitle: nextTitle });
    if (layout === ps.layout) continue;          // 没命中，下一个项目
    newStates.set(pid, { ...ps, layout });
    return { projectStates: newStates };         // paneId 全局唯一，命中即收工
  }
  return state;                                  // 一个都没命中：什么都不改
})
```
关键点（原版注释 `:1161-1162`）：
- **按 paneId 全局找** —— 移动端只认得 pane，不知道它挂在哪个项目下；
- **不落盘** —— pane 级 `customTitle` 不进 `savedLayout`，AI 会话本来就活不过重启。

**GPUI 签名**：
```rust
/// 移动端改会话名：按 pane_id 全局定位（移动端不知道 pane 挂在哪个项目下）。
/// 空串 = 清除自定义名，回落 shell 名。**不落盘**（SavedPane 里没有这个字段）。
pub fn rename_pane_by_id(&mut self, pane_id: &str, title: &str, cx: &mut Context<Self>)
```
实现：遍历 `self.project_states.values_mut()` → `layout.pane_mut(pane_id)`（`tree.rs:260`）→
命中就改 `custom_title` 并 `cx.notify()` 后 return。
⚠️ 与现有的 `AppStore::rename_pane(project_id, pane_id, title, cx)`（`store.rs:1310-1330`）**并存**：
那个是 F2/右键改名（知道项目），这个是移动端来的（不知道）。
现有那个做了 `title.trim()`，而移动端来的标题**已经在 mt-relay 里 sanitize 过了**（`relay.rs:709-716`），
再 trim 一次无害，但别在这条路上加别的收敛（比如再限一次长度）。

### 5.2 `mobile_relay` 配置写入（原版靠 `setConfig` + `withMobileRelayDefaults`）

`AppConfig::mobile_relay: Option<MobileRelayConfig>`（`crates/mt-config/src/config.rs:187`），
`MobileRelayConfig { relay_url, desktop_key, launchers }`（`:218-231`，`Default` 在 `:233-241`），
`load_config` 的迁移已保证整块存在（`:633-635`）。

需要两个方法（都走「只改指定字段、其余保持」的 `withMobileRelayDefaults` 语义）：
```rust
pub fn mobile_relay(&self) -> MobileRelayConfig                     // 缺省用 Default（含预置两条启动器）
pub fn set_mobile_relay_endpoint(&mut self, url: &str, key: &str, cx: &mut Context<Self>)
pub fn set_launchers(&mut self, launchers: Vec<AiLauncher>, cx: &mut Context<Self>)
```
⚠️ `mt_config::AiLauncher`（`config.rs:249-258`）与 `mt_relay::host::AiLauncher`（`host.rs:33-43`）
是**两个同形但不同 crate 的类型**（mt-relay 刻意不依赖 mt-config，`host.rs:29-32`）——
`RelayHost::launchers()` 里要做一次逐字段转换，别想着 `From` 自动来（可以自己写个 `fn to_relay(l: &mt_config::AiLauncher) -> mt_relay::AiLauncher`）。

### 5.3 `PaneStatus::as_str`

`crates/mt-app/src/tree.rs:36-63` 只有 `from_str`。快照要发状态串，补一个对称的：
```rust
pub fn as_str(self) -> &'static str {
    match self { Self::Idle => "idle", Self::AiIdle => "ai-idle", Self::AiWorking => "ai-working", Self::Error => "error" }
}
```
（与 `SplitDirection::as_str`，`tree.rs:166-171`，同一写法；**加个 round-trip 单测**钉住两个方向。）

### 5.4 `pane_of_pty` 反查（写穿要用）

`write_to_pane` 要 `(project_id, pane_id)`，而 mt-relay 只给 `pty_id`。加：
```rust
pub fn pane_of_pty(&self, pty_id: u32) -> Option<(String, String)>
```
遍历 `project_states` → `layout.panes()`（`tree.rs:235`）找 `pane.pty_id == Some(pty_id)`。
（`clear_preedit_of_focused`，`store.rs:568-580`，已有同形遍历可参考。）

### 5.5 `mobile_relay_status` 运行时字段

见 §2.1。**不落盘**，与 `focused_pane_id` 同类（纯运行时）。

---

## 6. GPUI 现状差异清单

| # | 差异 | 现状 | 本批处理 |
|---|---|---|---|
| 1 | mt-relay 零接线 | `mt-relay.workspace = true` 已在 `Cargo.toml:23`，源码里一次 `use` 都没有 | 本批的主体 |
| 2 | 边条没有「移动端」按钮 | `activity_bar.rs:16-21` 明写「原版 8 个按钮里 SSH / 移动端 / Git / 更新提醒四个在 GPUI 侧还没有对应功能，**不放占位**」 | 补一个。图标照抄原版 `ICON_MOBILE`（`src/components/ActivityBar.tsx:48-53`）：`<rect x=4.5 y=1.5 w=7 h=13 rx=1.5/>` + `<path d="M7 12.5h2"/>`，viewBox 16、stroke 1.2 —— 用 `Shape::line(Ink::Current, STROKE, Geom::Rect{..})` + 一条短横线，落进 `activity_bar.rs` 的常量表；按钮位置在「设置」之后（原版 `ActivityBar.tsx:167-170`），tooltip key `app.activityBar.mobile`（**字典里已有**，`dict.rs:25`/`:69`） |
| 3 | 没有 `mobileRelayStatus` 状态位 | store 里无 | §5.5 |
| 4 | 没有 `renamePaneById` | 只有带 project_id 的 `rename_pane`（`store.rs:1310`） | §5.1 |
| 5 | `PaneStatus` 缺 `as_str` | `tree.rs:55` 只有 `from_str` | §5.3 |
| 6 | 项目分组 UI 没做 | audit #13；但 `config.project_tree` 数据在、`mt_config::ProjectTreeItem` 也在 | `group_path` **照常组装**（数据齐全，与 UI 无关） |
| 7 | `ssh_connection_id` 恒 `None` | mt-ssh 未进 crates/，`ProjectConfig::ssh_connection_id` 字段**在**（`config.rs:386`）但没人写 | 快照里原样读该字段即可（永远是 None），**不必特判**；将来 SSH 批接上自动生效 |
| 8 | 无 toast kind 体系 | `main.rs:140-141` 只有 Completion/Attention 两个去重键 | 加 `MobileSessionToast` 键类型 |
| 9 | 无系统确认框 | 原版 `ask()` 走 Tauri dialog | 走 `prompt::Confirm`（`prompt.rs:154-207`） |
| 10 | 无 QR 库 | 工作区无任何 QR 依赖（`Cargo.toml:24-64` 全表可查） | §3.3 方案 A 引 `qrcode`（`default-features = false`），或方案 B 降级 |
| 11 | 输入框无 password 模式 | 待核对 `gpui_component::input::InputState` | §3.2 控件 5 |
| 12 | 无 `useEverOpened` / lazy 加载 | 原版为体积把三个重弹窗懒加载（`App.tsx:42-45`） | GPUI 不需要，删掉这一层 |

---

## 7. i18n key 清单

### 7.1 `mobileRelay` 命名空间：41 条，**双语已全在字典里**

`crates/mt-i18n/src/dict.rs:437-479`（zh）/ `:481` 起（en），命名空间注册在 `:1771-1774`。
TS 源头 `src/i18n/locales/mobileRelay.ts`。**一条都不用新增。**

```
apply                          clear                          intro
keyHint                        keyLabel                       keyPlaceholder
launchers.add                  launchers.cancel               launchers.commandPlaceholder
launchers.commandWarning       launchers.defaultShell         launchers.delete
launchers.edit                 launchers.empty                launchers.intro
launchers.namePlaceholder      launchers.save                 launchers.title
modal.generateQr               modal.needConnected            modal.notConfigured
modal.notPaired                modal.paired                   modal.pairedLabel
modal.pairedUnknown            modal.qrHint                   modal.qrWaiting
modal.regenerateQr             modal.resetConfirm             modal.resetPairing
modal.title                    status.authFailed              status.connected
status.connecting              status.disconnected            status.keyNotConfigured
status.reconnecting            status.versionMismatch         statusLabel
urlLabel                       urlPlaceholder
```
带插值的只有一条：`status.versionMismatch`（`{actual}` / `{expected}`）。

### 7.2 其他命名空间借用的 4 条（均已在字典里）

| key | 用处 | 位置 |
|---|---|---|
| `app.activityBar.mobile` | 边条按钮 tooltip | `dict.rs:25` / `:69` |
| `app.mobileStartSession` | 发起会话 toast，带 `{launcher}` | `dict.rs:40` / `:84` |
| `prompt.confirm` / `prompt.cancel` | 重置配对确认框的按钮（`Confirm::new` 的默认值，`prompt.rs:160-161`） | 已在 `USED_KEYS` |

### 7.3 必做的登记

新增的每个 `t()` 调用点都要往 `crates/mt-app/src/i18n.rs:122-264` 的 `USED_KEYS` 里加一行，
**字典序、不重复**（`i18n.rs:288-294` 有测试钉住）。共计新增约 43 行
（41 条 mobileRelay + `app.activityBar.mobile` + `app.mobileStartSession`）。

⚠️ `status.*` 那 7 条是**按状态串选文案**的，不要拼字符串 key ——
写成 `match status { "connected" => t("mobileRelay","status.connected"), .. }`，
这样 `USED_KEYS` 的 grep 抓得到，字典缺条目时测试会红。

### 7.4 若走 §3.3 方案 B（降级为复制链接）

需要在 **TS 源头** `src/i18n/locales/mobileRelay.ts` 补新词条（如 `modal.copyPairLink`），
再 `node crates/mt-i18n/tools/gen_from_ts.mjs` 重生成字典，并把
`crates/mt-i18n/tests/consistency.rs` 的条目总数对账常量改成新数目
（流程见 `crates/mt-app/src/i18n.rs:22-31`）。**不许手加进 `dict.rs`** —— 下次重生成就没了。

---

## 8. 样式要点

全部对照 `src/styles.css` 的 CSS 变量 → `crates/mt-app/src/ui.rs:36-56` 的 `Palette` 字段：

| CSS 变量 | `ui::` 取色函数 | 用在哪 |
|---|---|---|
| `--bg-base` | `bg_base()` | 输入框底 / 行容器底 |
| `--bg-surface` | `bg_surface()` | 草稿表单内输入框底（`AiLauncherSection.tsx:159`） |
| `--text-primary` / `--text-secondary` / `--text-muted` | 同名函数 | 正文 / 次要 / 说明 |
| `--accent` / `--accent-muted` | `accent()` / `accent_subtle()` | 主按钮、focus 边框 |
| `--border-subtle` / `--border-default` | 同名函数 | 行边框 / 输入框边框 |
| `--color-error` | `color_error()` | 删除/重置按钮、启动器空列表警告 |
| `--color-success` | `color_success()` | 状态徽章 connected |
| `--color-ai-working` | `color_ai_working()` | 状态徽章 connecting/reconnecting、**命令识别警告文字** |

尺寸口径：
- 面板宽 440px；二维码 260×260；圆点 8px；图标 16px；
- `text-base` ≈ 13px（对照 `modal.rs` 里普遍用的 `px(13.0)`），`text-sm` ≈ 11px（`modal.rs:202`、`:633`）；
- 圆角：`--radius-sm` ≈ 4px（`modal.rs:159`），`--radius-md` ≈ 6px；
- 行内 gap 8px（`gap-2`）、段间 16px（`space-y-4`）、列表行间 6px（`space-y-1.5`）。

按钮：ghost / primary / danger 三种现成的在 `ui.rs:320` / `:339` / `:361`，**直接用**，
只有「重置配对」那种「ghost 底 + error 文字 + hover error 边框」是现有三种之外的第四款，
可以在 `ui.rs` 加一个 `danger_ghost_button`，或就地拼。

---

## 9. 坑（按危害排序）

### 坑 1 —— 回调在 tokio 线程上来，直接碰 `Entity` 会 panic（或者根本编不过）

`RelayHost` / `RelayEvents` 的十个方法**全部**在 mt-relay 的 tokio 运行时上被同步调用
（`host.rs:14-15`、`relay.rs:511`/`:843` 两条任务）。
`Arc<dyn RelayHost>` 要求 `Send + Sync + 'static`，而 gpui `Entity` 不是 `Send` —— 编译期就会拦下你，
但**拦不住你把 `Mutex<something>` 塞进去然后在回调里做重活/长阻塞**。
正确做法只有一条：**四个 events + `write_pty` 走 channel 回主线程（§1.5.2），
另外三个 AI 查询直接透传 `AiBridge`（它内部全是 Arc+Mutex，本来就跨线程安全），
`launchers` / `project` 走主线程刷新的镜像快照。**

### 坑 2 —— 密钥缺失是 fail-closed，「连不上」不一定是网络问题

中转未配置 `MT_RELAY_DESKTOP_KEY` 时**拒绝一切桌面连接**（CLAUDE.md 移动端段）。
三种拒绝各有各的修法，**状态串不能合并**（`relay.rs:1124-1135` 有 pin 测试）：
`authFailed`（密钥填错）/ `keyNotConfigured`（中转没设）/ `versionMismatch`（版本不匹配）。
这三态与 `disconnected` 的本质区别是：**已停止重连**（`relay.rs:528-541` 三个分支都 `return`），
UI 必须把它们画成红色终态并给出可操作的文案，否则用户会一直等一个永远不会来的重连。

### 坑 3 —— 镜像绑定必须跳过 opencode / pi，否则串台

`crates/mt-relay/src/lib.rs:24-27` 的红线：hook 上报过会话身份就只认那一个会话文件；
没有身份时，**只有确实会写我们认识的会话记录的 agent（claude / codex / grok）才退启发式**。
opencode / pi 这类必须给空镜像 —— 退启发式会绑到同项目里别家的最新会话文件，
把别人的对话贴到这个 pane 上（比空镜像更糟）。
这条判定已经在 `relay.rs:866-874` 里了（走 `mirror::agent_has_session_log`），
**上层要做的是别把它绕开**：`RelayHost::ai_session_agent` 必须**如实**返回输入检测到的 agent 名
（`tracker.rs:260`），不要为了「让镜像有东西看」返回 `None` 或伪造成 `"claude"`。

### 坑 4 —— 重连退避有上限也有「不退避」的一档

`backoff_delay`（`relay.rs:970-973`）：1→2→4→8→16→32→60s 封顶。
但 `connection_loop`（`relay.rs:542`）对 **`ConnectedThenLost` 把 attempt 重置成 1** ——
握手成功过再断线视为网络抖动，1s 后就重来；只有「连都连不上」才逐级退避。
**别在 UI 上加「重连」按钮去调 `apply`**：那会把连接循环整条掐掉重建（`relay.rs:435-437` 先 `cancel`），
在 `versionMismatch` / `authFailed` 这类终态下确实需要（用户改完配置点「保存并连接」），
但在 `reconnecting` 状态下点它只会把退避进度清零、更慢连上。

### 坑 5 —— `start_session` 的每一条 return 都要先回执

见 §1.6 硬要求 2。原版 `mobileStartSession.ts` 里 5 处失败分支（`:61`、`:72`、`:80`、`:116`）
+ 1 处成功（`:129`）**全都调了 `reportResult`**。GPUI 侧用 `?` 早退时极容易漏 ——
建议把整个处理写成一个返回 `Result<String, StartSessionFailReason>` 的内层函数，
外层统一 `match` 之后调一次 `start_session_result`，**结构上杜绝漏回执**。

### 坑 6 —— `can_start_session` 对本批的 SSH 项目会误判为 true

`can_start_session(path, ssh_connection_id)`（`relay.rs:493-495`）= `ssh_connection_id.is_none() && !is_wsl_unc_path(path)`。
GPUI 侧 `ssh_connection_id` 恒 `None`（§6.7），所以**只有 WSL UNC 根项目会被正确置灰**。
这不是本批的 bug（远程项目本来就还开不出来），但**别为此在快照里造假值** ——
照实读 `ProjectConfig::ssh_connection_id`，SSH 批接上就自动对了。

### 坑 7 —— `launchers` 的 command / shell 绝不能进任何发出去的结构

ADR 0002。`send_snapshot`（`relay.rs:415-430`）已经只取 id+name；
**危险在上层**：别在日志、错误消息、状态载荷里带上 command
（比如「启动失败：{command} 起不来」这种文案回执给移动端）。
`StartSessionFailReason` 是**闭集枚举**，不带自由文本，保持这样。

### 坑 8 —— 配置写盘的并发与令牌

原版是 `await saveConfigToDisk(newConfig)` 之后才 `invoke(apply)`。
GPUI 的 `save_config_soon` 是 500ms 防抖（`store.rs:1459`），
**面板的保存动作要用 `save_config_now()`**（`store.rs:1480-1500`，带令牌过期重读重写）——
否则用户点完「保存并连接」立刻关掉应用，地址就丢了。
启动器 CRUD 同理。

### 坑 9 —— 结构同步的去抖别退化成「每帧发一次」

原版 150ms 去抖 + JSON 内容去重两道闸（`mobileSessionSync.ts:87-92`、`:99-102`）。
GPUI 的 `cx.observe(&store)` 在**每次 `cx.notify()`** 时都会触发，而 store 的 notify 频率
远高于 zustand 的 `subscribe`（终端状态、焦点、布局全在同一个 entity 上）。
两道闸一个都不能省，否则 WebSocket 上会出现每秒几十条 `SessionsDelta`。

### 坑 10 —— `aiPaneIds` 是跨调用状态，不是纯函数

`mobileSessionSync.ts:41-42` 那个模块级 `Set` 决定了「error 态的 pane 还算不算 AI pane」。
做成局部变量的话，AI 会话崩溃后 pane 会立刻从手机列表里消失，
用户在手机上看到的是「会话凭空没了」而不是「会话出错了」。

---

## 10. 验收清单

- [ ] 启动时若 `config.mobileRelay.relayUrl` 非空则自动建连（对照 `src-tauri/src/lib.rs:119-127`）；
- [ ] 面板七种状态各自的颜色与文案正确（含三种红色终态）；
- [ ] 「保存并连接」→ 落盘 → 重建连接；「断开并清除」→ 地址清空但**密钥与启动器保留**；
- [ ] 已连接时能生成二维码，扫码可完成配对；`paired === true` 时才出现「重置配对」；
- [ ] 重置确认框 → 取消不动作、确认后配对状态转 false；
- [ ] 启动器增/删/改后手机弹层立刻看到新名单（`launchers_changed` 生效）；
- [ ] 命令填 `npm test` 出黄字警告、填 `claude` 不出，且**照样能保存**；
- [ ] 手机发起会话 → 桌面后台新开 tab（**不切项目、不抢焦点**）+ 弹 toast + 手机拿到成功回执；
- [ ] 手机改会话名 → 桌面 tab 标题立刻变；改成空串 → 回落 shell 名；
- [ ] 手机发指令 → 目标终端像本人敲入一样执行（**pane 能正常进入 AI 会话状态**，这是写穿口是否正确的判据）；
- [ ] 项目分组层级在手机上还原正确（`groupPath`）；
- [ ] 单测：`rename_pane_by_id`（命中/未命中/空串）、`PaneStatus::as_str` round-trip、
      快照组装（全集项目 + 仅 AI pane + error 保留 + 深度优先序 + groupPath）、
      `relayHttpBase` 四种前缀；
- [ ] `cargo test -p mt-app` 全绿且**既有断言零改动**；`USED_KEYS` 两条测试通过。

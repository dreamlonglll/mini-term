# Research: SSH session pool patterns for `mt-ssh-mcp`

- **Query**: Lifecycle / eviction / keepalive / reconnect / concurrency / shutdown / cap patterns from real-world long-lived SSH session caches & DB connection pools, applied to a stdio MCP sidecar that wraps SSH.
- **Scope**: External (authoritative man pages, library source, RFCs) + internal (existing `mt-ssh-mcp.rs` design constraints).
- **Date**: 2026-05-22

---

## 0. Context anchors (so future readers don't have to re-derive)

- Sidecar lifetime: bound to **one stdio MCP client** (Claude / Codex). Process exits on stdin EOF — see `mt-ssh-mcp.rs:759` (`service.waiting().await?`).
- Sidecar process is **per-project** (`--project-id <id>`), agent calls tools serially in practice (rmcp tool handler is async but stdio is one-request-at-a-time on the wire), but parallel invocation cannot be ruled out for safety.
- Today every `ssh_exec` **spawns a fresh `ssh` child** (`run_ssh_piped` / `run_ssh_pty`). Cost is the full TCP handshake + KEX + auth (~hundreds of ms — seconds, badly amplified on jump hosts).
- Sidecar code already inhabits Tokio multi-thread runtime; PTY/blocking work is fenced behind `spawn_blocking` (`mt-ssh-mcp.rs:704`).
- `mt_core::SshConnection.password` is **already cached in memory** for the sidecar's lifetime (read from `config.json` on every call right now); so the design constraint "reconnect needs the password" is already satisfied.

---

## 1. TL;DR — recommended defaults

| Concern | Recommendation | Rationale anchor |
|---|---|---|
| SSH library | `russh` 0.x (pure Rust, native Tokio, no external `ssh` binary) | Already in Cargo ecosystem; example at `russh/examples/client_exec_simple.rs`; provides `keepalive_interval`, `inactivity_timeout`, `Disconnect::ByApplication`. |
| Idle timeout (close cached session after no exec for N) | **10 minutes**, configurable per sidecar via env / CLI | HikariCP default `idleTimeout=600_000ms`; matches typical coding-session attention span; well below NAT 2h 4min minimum (RFC 5382 REQ-5). |
| Max lifetime (hard recycle) | **2 hours** | Below OpenSSH `RekeyLimit` defaults' practical lifespan and below most middlebox NAT eviction; HikariCP recommends "several seconds shorter than DB-imposed limit"; for SSH there's no real driver-imposed limit — 2h is a defensive recycle. |
| Keepalive interval (protocol-level `SSH_MSG_GLOBAL_REQUEST keepalive@openssh.com`) | **30 seconds** | OpenSSH `ServerAliveInterval` defaults to 0 (disabled) but the **community-standard "always-on" value is 30–60s** for NAT-friendly behavior; cheap (a few bytes of encrypted ping). |
| Keepalive miss count → declare dead | **3** (i.e. 90s total before declaring dead) | Matches OpenSSH `ServerAliveCountMax=3` default; matches russh `keepalive_max=3` default. |
| Reconnect policy | **Lazy auto-reconnect on next `ssh_exec`** + **single retry** on "broken pipe" detected during exec | Sidecar is short-lived per coding session; auto-reconnect on detection avoids forcing the agent to retry; cap retry at 1 to avoid masking real auth failures. |
| Concurrency model | **One `tokio::sync::Mutex` per session, channels opened serially** | Simplest; agents call serially. Allowing parallel channels is a YAGNI feature (rmcp framework can serialize at the tool level too). Keeps reasoning about audit logging linear. |
| Max sessions cap | **8** (configurable). LRU eviction on cap. | A coding session rarely talks to >2–3 hosts. 8 leaves plenty of headroom while bounding file-descriptor + memory growth (russh keeps one tokio task + buffers per session ≈ ~256 KB). |
| Shutdown hook | On `service.waiting()` return, iterate sessions and `disconnect(Disconnect::ByApplication)`. Plus `Drop` impl as backstop. | russh provides `session.disconnect(Disconnect::ByApplication, "", "en")`; mirrors fabric `close()` semantics. |
| Re-auth on reconnect | Re-use the **cached** `mt_core::SshConnection` snapshot the session was built with. **Do not re-read `config.json`** on reconnect — that opens a window where a password change silently "kicks" an active connection. | Treat the cached connection as the binding contract for that session's lifetime. |
| Audit logging | Keep current behavior — write one line per `ssh_exec` to `ssh-mcp-audit.log`. Additionally log `session_open` / `session_close` / `session_reconnect` events to the same file. | Existing `format_audit_line` reusable; helps debug a flaky cached session post-mortem. |

---

## 2. Survey of tools (what each does + what we can borrow)

### 2.1 OpenSSH `ControlMaster` / `ControlPersist`

**Authoritative source**: `https://man.openbsd.org/ssh_config` — keywords `ControlMaster`, `ControlPath`, `ControlPersist`.

- `ControlMaster=yes`: a single `ssh` process opens the TCP/SSH session, listens on a Unix-domain control socket; subsequent `ssh` invocations with matching `ControlPath` reuse the master rather than handshaking again.
- `ControlPersist=yes`: master stays in the background **indefinitely** after the last client disconnects. `ControlPersist=600` (or `10m`): stays for 10 minutes after last client exits, then closes.
- Default `ControlPersist=no` means master exits as soon as the foreground client exits.

**What we borrow**: the **idle-timeout-after-last-use** pattern. Our equivalent is "session sits in the map; if no `ssh_exec` for N minutes, evict and close". OpenSSH offers a per-host-config knob; **we should expose this as a global sidecar setting only**, not per-saved-connection, to keep `SshConnection` minimal.

### 2.2 `autossh`

**Authoritative source**: `https://www.harding.motd.ca/autossh/` and `autossh(1)` man page (env vars `AUTOSSH_POLL`, `AUTOSSH_FIRST_POLL`, `AUTOSSH_GATETIME`, `AUTOSSH_PORT`).

- Wraps `ssh` and monitors it; on death **restarts** the child.
- Two health-check modes:
  1. Echo-port pair (forward + remote echo service) — `autossh` writes test data and times the round-trip;
  2. **Default since v1.4** rely on OpenSSH `ServerAliveInterval`/`ServerAliveCountMax` and just restart on child exit.
- `AUTOSSH_POLL` default **600 s** for the echo-port mode.
- `AUTOSSH_GATETIME` (default 30 s): if `ssh` exits within `GATETIME` of startup, autossh treats it as a hard failure and gives up.
- Backoff: rapid failures lead to wider restart intervals.

**What we borrow**: the **"trust SSH keepalive, just restart on death"** pattern + **gatetime-style detection of "this is fundamentally broken, stop retrying"**. Our equivalent: if the first `ssh_exec` on a freshly-rebuilt session fails immediately (e.g. auth error), don't loop — return the error to the agent and **mark the session unhealthy with a cooldown** so the next call doesn't hammer.

### 2.3 `mosh` (mobile shell)

**Authoritative source**: `https://mosh.org/`.

- UDP-based State Sync Protocol (SSP) **above** an initial SSH handshake. Once `mosh-server` is launched, the SSH connection terminates and SSP takes over.
- Detection of "dead" connection is cooperative: client sends periodic state-acks; server sends the latest screen. If the client doesn't hear back, the UI shows a warning **but keeps trying** — connection resumes when network returns.

**What we borrow**: **don't tear down on transient network blips**. We should treat brief network failures as "needs reconnect" rather than "client error". Concretely: on `ssh_exec` failure with a transport-layer error (broken pipe, timeout), perform **one auto-retry that re-establishes the session** before bubbling the error.

Caveat: SSP roaming is overkill for our use case (we control both endpoints' lifetime — sidecar dies when agent dies; agent doesn't migrate networks mid-session). The simple "drop + reconnect lazily" model is enough.

### 2.4 paramiko `SSHClient` / `Transport`

**Authoritative source**: `https://docs.paramiko.org/en/stable/api/transport.html`.

- `Transport.set_keepalive(interval)`: enables protocol-level keepalive every `interval` seconds. **Default off**. Used "to keep connections alive over a NAT".
- `Transport.is_active()`: returns whether session is open (cheap, in-process check; does NOT round-trip).
- `Transport.open_channel(timeout=3600)`: default channel-open timeout is **1 hour** — much higher than per-call timeouts (paramiko assumes most use cases hold long-running channels).
- No built-in pool. Application is expected to wrap with own caching.

**What we borrow**: 
1. The split between **"is_active (cheap, local)"** and **"actually round-trip a ping"** — analogous to sqlx's `test_before_acquire`. We'll cheap-check `Session::is_closed()` (russh equivalent) before issuing each exec; the keepalive timer handles deeper liveness.
2. **`set_keepalive` style API** — single `Duration` knob, default off. We'll default on (30s), but expose a knob.

### 2.5 fabric `Connection`

**Authoritative source**: `https://docs.fabfile.org/en/latest/api/connection.html` (and source at `fabric/connection.py`).

- Lifecycle: `__init__` → `open()` (lazy on first `run()`) → `run()` (re-opens automatically via the `@opens` decorator if not connected) → `close()` (manual or context-manager).
- Uses paramiko `SSHClient` under the hood; `is_connected = self.transport.active if self.transport else False`.
- Documents (with a `.. warning::`) that **garbage-collection-based close is unsafe** — "can lead to end-of-process hangs and similar behavior" — relies on explicit `close()`.

**What we borrow**:
1. **Lazy `open()` on first use** via a decorator-equivalent. In Rust we'd have `Pool::get_or_connect(&conn_view).await?` returning a guard that re-establishes if dead.
2. **Explicit shutdown — never trust Drop alone**. We need an explicit cleanup pass on sidecar shutdown (driven by `service.waiting().await?` returning). Drop is a backstop only.
3. **`is_connected` style cheap probe** — `Transport.is_active()` returns purely from in-memory state. russh has `Handle::is_closed()` (see `russh/src/client/mod.rs`).

### 2.6 HikariCP (Java JDBC pool — defaults source-of-truth for "DB pool")

**Authoritative source**: `https://github.com/brettwooldridge/HikariCP/blob/dev/README.md`.

Key knobs and defaults (all in ms):

| Knob | Default | Note |
|---|---|---|
| `idleTimeout` | **600_000** (10 min) | Only retires connections beyond `minimumIdle`. Min allowed 10 s. |
| `keepaliveTime` | 0 (disabled) | Ping cadence on idle connection; min 30 s; recommended **range of minutes**. |
| `maxLifetime` | **1_800_000** (30 min) | Hard close even if in use? No — only when closed. Recommended a few seconds **less than** any infrastructure-imposed timeout. |
| `maximumPoolSize` | 10 | Cap on simultaneous connections. |
| `connectionTimeout` | 30_000 | How long `getConnection()` will wait before throwing. |
| `leakDetectionThreshold` | 0 (off) | Logs warning if connection held > N ms; min 2_000. |

**What we borrow**:
- The **`idleTimeout=10min` default** is widely accepted as "reasonable for interactive workloads".
- The **`maxLifetime` defensive recycle** to dodge middlebox eviction without the app noticing.
- The **`keepaliveTime` "ping on idle"** pattern, but in our case it's not a query — it's an SSH global request, native to the protocol.
- Cap on pool size to bound resource usage.

### 2.7 `sqlx::pool::PoolOptions` (Rust analog of HikariCP)

**Authoritative source**: `https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html`.

- `idle_timeout(Option<Duration>)`: connection in idle queue longer than this is closed.
- `max_lifetime(Option<Duration>)`: hard recycle.
- `max_connections(u32)`: pool cap (mandatory).
- `acquire_timeout(Duration)`: max wait when caller blocks on `acquire()`.
- `test_before_acquire(bool)` (default **true**): cheap `ping()` before handing out a connection; can be replaced with `before_acquire` callback that only pings stale connections.
- `before_acquire(fn(&mut conn, meta) -> bool)`: custom predicate.

**What we borrow**: the API shape is very ergonomic for Rust. Our `SshPool` should expose similar methods (or a config struct) for `idle_timeout`, `max_lifetime`, `keepalive_interval`, `max_sessions`. A `test_before_acquire`-style **cheap `is_closed()` check** on every exec is essentially free and protects against the race where a server-side disconnect arrives between two exec calls.

### 2.8 `russh::client::Config` (the SSH library we should use)

**Authoritative source**: russh source at `russh/src/client/mod.rs` lines around the `Config` struct.

```rust
pub struct Config {
    pub client_id: SshId,
    pub limits: Limits,
    pub window_size: u32,           // 2_097_152 default
    pub maximum_packet_size: u32,   // 32_768 default
    pub channel_buffer_size: usize, // 100 default
    pub preferred: negotiation::Preferred,
    pub inactivity_timeout: Option<Duration>,   // None default
    pub keepalive_interval: Option<Duration>,   // None default
    pub keepalive_max: usize,                   // 3 default
    pub anonymous: bool,
    pub gex: GexParams,
    pub nodelay: bool,
}
```

russh already implements the core loop we need (in `russh/src/client/encrypted.rs` or session loop):

- `keepalive_interval`: when no data received from server for this long, send an `SSH_MSG_GLOBAL_REQUEST` "keepalive@openssh.com" with `want_reply=true`.
- `keepalive_max`: after this many keepalives without reply, return `Error::KeepaliveTimeout`.
- `inactivity_timeout`: after this long of zero traffic (including our own sends), return `Error::InactivityTimeout`.

**What we borrow**: set `keepalive_interval = Some(Duration::from_secs(30))`, `keepalive_max = 3`, **leave `inactivity_timeout = None`** (we manage idle eviction in the pool layer, where we can also tear down the tokio task cleanly).

### 2.9 OpenSSH `ServerAliveInterval` / `ServerAliveCountMax` (the spec russh implements)

**Authoritative source**: `https://man.openbsd.org/ssh_config`.

- `ServerAliveInterval` default **0** (disabled).
- `ServerAliveCountMax` default **3**.
- Worked example from man page: "if `ServerAliveInterval=15` and `ServerAliveCountMax=3` is default, ssh disconnects after approximately 45 s of unresponsiveness."
- Separate from `TCPKeepAlive` (default **yes**, but spoofable, OS-level).

**Why protocol-level beats TCP**: encrypted channel → can't be spoofed by a middlebox; works through NAT in both directions; detects "server kernel still ACKs TCP but SSH daemon is wedged".

### 2.10 RFC 5382 — NAT timeout requirements (why 30 s keepalive is reasonable)

**Authoritative source**: `https://datatracker.ietf.org/doc/html/rfc5382`.

- REQ-5: "the value of the 'established connection idle-timeout' MUST NOT be less than 2 hours 4 minutes." This is the **minimum** for RFC-compliant NATs.
- **Real-world reality**: many home routers, cellular carriers, and corporate NATs use 5–30 minutes; some as low as 60 seconds for UDP and a few minutes for "idle" TCP.
- Default Linux TCP keepalive: 2 hours idle, 75 s probe interval, 9 probes (per RFC 1122 recommendation).

**Implication**: We can't rely on RFC compliance from the user's home Wi-Fi router. **30 s SSH-level keepalive** is well below even pathological NAT timeouts and protects against this reliably.

---

## 3. Concrete recommended design

### 3.1 Data structures

```text
SshPool
├── inner: tokio::sync::RwLock<HashMap<ConnectionId, Arc<CachedSession>>>
├── config: PoolConfig            // idle_timeout, max_lifetime, keepalive_*, max_sessions
└── reaper_handle: JoinHandle<()> // background task

CachedSession
├── session: tokio::sync::Mutex<russh::client::Handle<Client>>  // serialize channel-open
├── opened_at: Instant            // for max_lifetime
├── last_used: AtomicU64          // millis since epoch; updated on every exec
├── conn_snapshot: SshConnection  // immutable copy used to (re)build this session
└── unhealthy_until: AtomicU64    // cooldown after auth fail (gatetime-style)
```

### 3.2 `ssh_exec` flow (new)

```text
1. find_connection(conn_view, project_id)  -> SshConnection snapshot
2. let session = pool.acquire(snapshot).await?;
     - if cached and !is_closed and now < unhealthy_until: reuse
     - else if cached but is_closed: rebuild via russh (auth using snapshot.password)
     - else (cold): build fresh, insert into map, evict LRU if size > cap
3. let channel = session.lock().await.channel_open_session().await?;
     - on Err: drop session from cache, retry once (single auto-reconnect)
     - on second Err: surface to agent + mark unhealthy for 30 s cooldown
4. channel.exec(true, remote_command).await?;
   collect stdout/stderr/exit (russh exposes them per-message in ChannelMsg)
5. last_used = now()
6. audit_log(conn.name, command, exit_code)
```

### 3.3 Idle timeout / eviction

- **Reaper task**: spawned on pool creation; ticks every **60 s** (cheap; configurable).
- For each entry: if `now - last_used > idle_timeout` **or** `now - opened_at > max_lifetime` → remove from map and `disconnect(Disconnect::ByApplication, "", "en")`.
- Defaults: `idle_timeout = 10 min`, `max_lifetime = 2 h`.
- **Override mechanism**: env vars `MT_SSH_POOL_IDLE_TIMEOUT_SECS`, `MT_SSH_POOL_MAX_LIFETIME_SECS`, `MT_SSH_POOL_MAX_SESSIONS` parsed once in `main()`. Reject sub-30-s values for keepalive and sub-10-s for idle_timeout to avoid foot-guns.

### 3.4 Keepalive

- Configured once on the russh `Config` at session build time:
  - `keepalive_interval = Some(Duration::from_secs(30))`
  - `keepalive_max = 3`  (matches russh's own default)
- russh's internal task fires `SSH_MSG_GLOBAL_REQUEST` automatically; we don't manage the timer.
- When russh returns `Error::KeepaliveTimeout` from any await on the session handle, the entry's next acquire will see `is_closed() == true` and rebuild.

### 3.5 Reconnect policy

- **Lazy rebuild** is the default: detection happens at `acquire()` time via `is_closed()`. No background reconnect task — saves us a tokio task per dead connection that the agent may never touch again.
- **Single auto-retry inside one `ssh_exec` call** if `channel_open_session()` or `exec()` returns a transport error after a successful `acquire()` (catches the race where the server FIN-acks between acquire and use).
- **No second auto-retry**: if both attempts fail, return the error and set `unhealthy_until = now + 30s`. Next agent call within 30 s gets immediate "session unhealthy" without hammering the server.

### 3.6 Concurrency

- **Per-session `tokio::sync::Mutex`** held around the `russh::client::Handle`. Channels are opened, used, and dropped serialized within a single session.
- Rationale:
  - russh `Handle` is `Clone` (it's just a `mpsc::Sender` to the session task) — multiple clones *can* call `channel_open_session()` concurrently in principle.
  - But mixing concurrent execs makes audit-log ordering ambiguous and complicates the "mark unhealthy on failure" logic.
  - Agents call serially in practice. **Parallel exec across the same SSH host is a YAGNI feature**.
- **Cross-session concurrency** is unimpeded — two different `connection_id`s acquire two different cached sessions, each with its own Mutex, and run truly in parallel via Tokio.

### 3.7 Shutdown hook

```rust
// in main(), after service.waiting().await?
eprintln!("[mt-ssh-mcp] client disconnected, draining session pool");
pool.shutdown().await;   // disconnect all + abort reaper
```

`pool.shutdown()`:
1. Abort the reaper `JoinHandle`.
2. `drain` the inner map; for each entry, spawn (with small concurrency limit) `session.disconnect(Disconnect::ByApplication, "", "en").await` with a per-session 2-second timeout. Don't block sidecar exit on a misbehaving remote.
3. Also implement `Drop for SshPool` that aborts the reaper as a backstop; we cannot do async work in Drop, so server-side close is best-effort there.

**SIGTERM/SIGINT note**: stdio MCP clients almost always exit by closing stdin; `service.waiting()` returns and `main` proceeds to shutdown naturally. On forcible kill (SIGKILL on Unix, `TerminateProcess` on Windows) we get no chance to clean up — the SSH server will see TCP reset and tear down its end. Acceptable; this is the same failure mode the current `Stdio::null()` path would have.

### 3.8 Max sessions cap + LRU eviction

- Default cap: **8**.
- Eviction strategy: simplest correct option is **"least-recently-used by `last_used`"**. On `acquire()` for a new connection ID when the map is full, pick the entry with the smallest `last_used`, disconnect it, then insert the new session.
- Sub-30-second LRU thrash is theoretically possible if an agent rapidly fan-outs across more hosts than the cap; in practice 8 is comfortably above any realistic coding-session host count.

### 3.9 Auth re-prompt on reconnect

- **No re-prompt**. The cached `SshConnection.password` is the password contract for the sidecar's lifetime.
- Trade-off: if the user changes the password in mini-term UI mid-session, the old cached password keeps working until the session is evicted. This is **the same trade-off** OpenSSH `ControlMaster` makes — the master holds its own credentials.
- Rationale: re-prompting via the agent would require a tool round-trip ("password changed, please re-confirm"), which is hostile UX. Sidecars are short-lived; users restart Claude/Codex anyway after editing connection details.

---

## 4. Failure mode matrix

| # | Scenario | Detection mechanism | Pool behavior |
|---|---|---|---|
| a | **Remote server reboots** (TCP RST on first reconnect attempt, then full handshake fails until `sshd` is back) | russh task notices peer FIN/RST → `Handle::is_closed() == true` on next `acquire()`. Reconnect attempt fails with `Error::Disconnect` or TCP `ConnectionRefused`. | Evict cached entry; first `ssh_exec` after reboot returns error to agent; agent retries → after `sshd` ready, fresh session built transparently. |
| b | **NAT drops idle conn after 5 min** (carrier-grade NAT silently expires the conntrack entry) | russh keepalive every 30 s sees no reply → at 90 s (3 missed) returns `Error::KeepaliveTimeout` → session task ends → `is_closed() == true`. | Same as (a): evict on next acquire, rebuild. **Total agent-visible downtime: ≤90 s for keepalive detection + ~handshake duration.** |
| c | **Brief network flap** (Wi-Fi blip <30 s) | If flap completes before next keepalive: TCP keeps the connection (kernel retransmits). If keepalive lands in the gap: 1 missed keepalive, **session survives** (count=1 < max=3). | No eviction; transparent to agent. This is the **dominant motivation** for keepalive in the first place. |
| d | **User changes saved password but old session still cached** | None — by design. Pool uses snapshot from first build. | Cached session keeps working with old password. New password takes effect only after natural eviction (idle 10 min / lifetime 2 h) or sidecar restart. **Documented trade-off**, not a bug. To force immediate refresh, user restarts the AI agent or we expose an `ssh_disconnect` tool (out of scope for this task). |
| e | **Sidecar SIGKILL'd** | None on our side — process is dead. | Server sees TCP RST, tears down channels. Audit log loses the trailing exec record (no flush guarantee on SIGKILL, but each `append_audit_log` call is `O_APPEND` + `write`, so each *completed* exec is durable). Acceptable. |
| f | **Auth failure on rebuild** (e.g. password rotated server-side, key revoked) | First exec after eviction returns auth error from russh. | Mark `unhealthy_until = now + 30s`. Next 30 s of `ssh_exec` calls return the cached error without re-hitting the server (gatetime pattern). After 30 s, retry — gives user time to fix in mini-term UI and reconnect. |
| g | **Agent invokes two `ssh_exec` concurrently on same connection** (rare but possible) | Both arrive at `pool.acquire()` → both get the same `Arc<CachedSession>` → contend on the Mutex. | Second exec waits behind first — serialized. Audit log entries are ordered. Throughput hit is real (one exec at a time per host) but correctness preserved. If contention becomes painful, switch to per-channel concurrency (russh supports it) — not now. |
| h | **Sidecar startup with stale `config.json`** | Each `ssh_exec` already re-reads `config.json` today (`read_ssh_connections_for_project` on every call). Should we keep that, or snapshot at sidecar startup? | **Keep current behavior of reading on every call for the *lookup*** (so connections added during session are visible). But once a session is built, **freeze the `SshConnection` into the cache** — don't re-resolve mid-session. This is consistent with (d). |

---

## 5. Risks & open questions

1. **Windows `ssh` binary vs russh native**: today we shell out to OpenSSH for both the piped and PTY paths. Moving to russh removes the PTY-autofill complexity entirely (russh authenticates with a password directly — no PTY scraping needed; `session.authenticate_password(user, password)`). This is a **net simplification** for the codebase, **but** loses one thing: `ProxyJump` via `-J`. russh supports tunneling-through-jump-host via `channel_open_direct_tcpip` (open a TCP channel through host A to host B, then start a new russh session over that channel), but the implementation cost is non-zero. Verify before committing: scope task to "no proxy jump in v1" or build the wrapper.
2. **Audit log volume** with persistent sessions: same as today (one line per exec). No change.
3. **Memory growth**: each russh session holds buffers per `window_size` (default 2 MB). 8 sessions × 2 MB = 16 MB worst-case. Acceptable.
4. **`russh::client::Handle::is_closed()` semantics**: it checks whether the local `mpsc` sender is still attached to a live task. If the task panicked, `is_closed()` returns true. Verify via russh source — see `russh/src/client/mod.rs:Handle::is_closed`. (Already located in this research; matches expectation.)
5. **No `ssh_disconnect` MCP tool** in scope: if user explicitly wants to nuke a cached session, they restart the agent or kill the sidecar. Could add later.

---

## 6. Files / references

### Internal (mini-term repo)

| Path | Why relevant |
|---|---|
| `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs` | Current implementation; lines 411-560 (`run_ssh_pty`) and 337-400 (`run_ssh_piped`) become obsolete once we move to russh. `tool_router::ssh_exec` (lines 654-720) becomes the integration point with `SshPool::acquire`. |
| `src-tauri/mt-core/src/ssh_connection.rs` | `SshConnection` is the snapshot we'll cache in `CachedSession`. |
| `src-tauri/mt-core/src/ssh_prompt.rs` | Becomes **unused** once russh handles auth natively (no more PTY prompt scraping). Keep around for the main app's terminal PTY use case. |
| `src-tauri/mt-core/src/ssh_key.rs` | `prepare_ssh_key` (tightened-permission temp copy) still used for the **key-based** path — russh accepts a `PrivateKey` directly, so we read & parse the file in-process; the permission tightening only matters for the OpenSSH client binary. Likely simplifies to "load and parse key file directly with `russh::keys::load_secret_key`". |
| `src-tauri/mt-sidecars/Cargo.toml` | Add `russh = "0.x"`, `russh-keys = "0.x"`. `portable-pty` becomes unused for `mt-ssh-mcp` (still used by `miniterm-hook`). |
| `.trellis/spec/backend/index.md` | Update the "BatchMode=yes" gotcha — once we move off the `ssh` binary, BatchMode no longer applies to the MCP path. Note this as a follow-up doc edit. |
| `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs:759` | `service.waiting().await?` is the hook point for shutdown drain. |

### External (authoritative)

| URL | What it gives |
|---|---|
| `https://man.openbsd.org/ssh_config` | `ControlMaster`, `ControlPersist`, `ServerAliveInterval`/`Count`, `TCPKeepAlive`, `ConnectTimeout` definitions and defaults. |
| `https://docs.paramiko.org/en/stable/api/transport.html` | `set_keepalive`, `is_active`, `open_channel(timeout=3600)`. |
| `https://docs.fabfile.org/en/latest/api/connection.html` (or source `fabric/connection.py`) | `Connection.open/close/is_connected` lifecycle; warning about GC-based close. |
| `https://www.harding.motd.ca/autossh/` and `autossh(1)` man | `AUTOSSH_GATETIME` (default 30 s), `AUTOSSH_POLL`, monitor modes. |
| `https://mosh.org/` | Roaming / intermittent-connectivity philosophy ("don't tear down on blips"). |
| `https://github.com/brettwooldridge/HikariCP/blob/dev/README.md` | `idleTimeout=10min`, `maxLifetime=30min`, `keepaliveTime` semantics, `maximumPoolSize=10`. |
| `https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html` | Rust idiomatic pool API: `idle_timeout`, `max_lifetime`, `max_connections`, `acquire_timeout`, `test_before_acquire`, `before_acquire`. |
| `https://docs.rs/russh/latest/russh/client/struct.Config.html` and source `russh/src/client/mod.rs` | `keepalive_interval`, `keepalive_max=3`, `inactivity_timeout`. |
| `https://github.com/Eugeny/russh/blob/main/russh/examples/client_exec_simple.rs` | Reference for `Session::connect`, `authenticate_publickey`, `channel_open_session`, `disconnect(Disconnect::ByApplication, "", "en")`. |
| `https://datatracker.ietf.org/doc/html/rfc5382` | NAT REQ-5: ≥2h4min idle-timeout for compliant NATs; real-world routers much shorter; justifies 30 s keepalive. |
| `https://datatracker.ietf.org/doc/html/rfc4254` | SSH connection-protocol channels — multiple parallel channels per session are legal, informs the "concurrency" decision. |

---

## 7. Caveats / not-found

- No authoritative published **default for `ControlPersist`** chosen by major distros — different distros patch the default differently. The man page default is `no`; common community advice picks 5–10 min.
- Did not exhaustively benchmark **russh handshake cost** vs `ssh` binary cost — both are bound by network RTT + KEX (Curve25519 ≈ <10 ms CPU), so they should be comparable; the pool's win is in **amortizing** that cost across N execs.
- `gh` CLI tunnel persistence patterns referenced in the question — couldn't find authoritative documented defaults; `gh codespace ssh` and similar appear to delegate to OpenSSH `ControlMaster` under the hood. Effectively covered by §2.1.
- Did not verify whether **`russh` supports `ProxyJump`-style chained connections** out-of-the-box for our v1 — flagged in §5 risks as something the design needs to verify before implementation.

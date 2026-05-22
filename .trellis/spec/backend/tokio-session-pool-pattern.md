# Tokio 常驻资源池骨架

> 在 sidecar / 长跑服务里维护「按 key 缓存的常驻资源」（SSH session、HTTP/2 连接、SFTP client、数据库连接……），并配上**后台 reaper + graceful shutdown**。本文沉淀 `mt-ssh-mcp` 落地后被验证的最小可复用骨架，新写同类池子时照本宣科即可。
>
> **不适用**于：纯 stateless 缓存（用 `tokio::sync::RwLock<HashMap>` 即可）、不要 reaper / 不要 shutdown 钩子的轻量缓存、tokio 之外的运行时。

---

## 什么时候用这个 pattern

- 资源建立成本高（SSH/TLS 握手、auth），希望按 connection-key 缓存复用。
- 资源有自然「过期」：idle 太久 / 活太久（NAT 静默丢链） / 远端主动断。
- 进程退出前要 graceful 关掉所有资源（避免远端留 zombie / fd 泄漏）。
- 资源数量需要封顶（防泄漏 / OOM）。

---

## 数据结构

```text
Pool                                      ── 池的对外 facade
├── inner:  Arc<tokio::sync::Mutex<Inner>>     ── 真正的 K→V map + acquire/evict 临界区
├── config: PoolConfig                          ── idle / lifetime / keepalive / cap / cooldown 参数
└── reaper: std::sync::Mutex<Option<JoinHandle<()>>>
                                                ── 后台 reaper task 句柄;Drop 用同步 abort

Inner                                     ── 池内部状态
└── sessions: HashMap<Key, Arc<Resource>>      ── 真正的缓存

Resource                                  ── 池里一项
├── handle:           tokio::sync::Mutex<<...库特定...>>
                                                ── 串行化对底层资源的操作
├── opened_at:        Instant                   ── 用于 max_lifetime 判定
├── last_used:        AtomicU64                  ── UNIX 毫秒;reaper 不抢锁就能读
└── unhealthy_until:  AtomicU64                  ── 0 = 健康;否则截止 UNIX 毫秒
                                                ── 见「关键决策 5」
```

---

## 关键决策

### 1. 后台 reaper 持 `Weak<Mutex<Inner>>`，不是 Arc

```rust
let inner = Arc::new(Mutex::new(Inner { sessions: HashMap::new() }));
let weak = Arc::downgrade(&inner);
let handle = tokio::spawn(async move {
    let mut ticker = tokio::time::interval(tick);
    ticker.tick().await; // 首次 tick 立即返回,跳过
    loop {
        ticker.tick().await;
        let Some(arc) = weak.upgrade() else { return; }; // Pool drop 后 task 自然退出
        // ... 拿锁,选过期项,从 map 移除,异步外抛 disconnect ...
    }
});
```

**Why**：如果 reaper task 持的是 `Arc<Mutex<Inner>>`，pool 拥有的那一份 Arc 即便 drop，reaper 还活着 → strong_count 永远 ≥ 1 → inner 永远不释放 → 内存泄漏 + task 阻碍进程退出。换成 `Weak`，pool drop 后 strong_count 归零，下一次 `upgrade()` 返 `None`，task 立刻 `return`。

### 2. `JoinHandle` 用 `std::sync::Mutex` 而非 `tokio::sync::Mutex`

```rust
pub struct Pool {
    inner: Arc<tokio::sync::Mutex<Inner>>,
    reaper: std::sync::Mutex<Option<JoinHandle<()>>>, // 注意：std,不是 tokio
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(mut g) = self.reaper.lock() {
            if let Some(h) = g.take() { h.abort(); }
        }
    }
}
```

**Why**：`Drop` 是同步上下文，**不能 `.await`** 去拿 tokio 锁。`JoinHandle::abort()` 本身是同步 API，标准库 `Mutex` 即可。这一对设计保证「Drop 时无脑兜底 abort 掉 reaper」始终能跑成功，与是否在 tokio runtime 内无关。

### 3. `shutdown()` 三步，**不能持锁 await**

```rust
pub async fn shutdown(&self) {
    // 1) abort reaper
    if let Ok(mut g) = self.reaper.lock() {
        if let Some(h) = g.take() { h.abort(); }
    }
    // 2) drain map（持锁短时间）
    let entries: Vec<_> = {
        let mut inner = self.inner.lock().await;
        inner.sessions.drain().map(|(_, v)| v).collect()
    }; // 锁在这里 drop
    // 3) 释放锁后再 join_all 异步 disconnect,每条加 per-session 超时
    let futures = entries.into_iter().map(|s| async move {
        let _ = tokio::time::timeout(
            shutdown_per_session_timeout,
            async {
                let h = s.handle.lock().await;
                let _ = h.disconnect_or_close().await;
            },
        ).await;
    });
    futures::future::join_all(futures).await;
}
```

**Why 三步分离**：
- **step 2 持锁 await 是反模式**：若边 hold lock 边 disconnect，每条资源 disconnect 期间整个 pool 无法 acquire / evict / 被 reaper 触碰。disconnect 慢一点（网络抖动）→ acquire 卡死 → 调用方超时 → 雪崩。
- **step 3 join_all 并发**：N 条资源 disconnect 同时跑，整体耗时 = 最慢一条（封顶 = `shutdown_per_session_timeout`），不是累加。
- **每条加 per-session timeout**：远端 hang 也不阻塞 sidecar 退出。

### 4. `acquire()` fast-path 用短锁查命中

```rust
pub async fn acquire(&self, key: &Key) -> Result<Arc<Resource>, Err> {
    // fast-path: 短锁,只查命中
    {
        let inner = self.inner.lock().await;
        if let Some(s) = inner.sessions.get(key) {
            if !is_dead(s) {           // 只看 transport 是否还活
                s.touch();
                return Ok(s.clone()); // **原样返回,包括带 unhealthy 标记的**
            }
        }
    } // 锁在这里 drop
    // slow-path: 建新资源(可能耗时秒级,不持锁)
    let new_res = self.build(key).await?;
    let arc = Arc::new(new_res);
    // 拿锁插入 + LRU 淘汰
    let mut inner = self.inner.lock().await;
    inner.sessions.remove(key); // 把可能已死的旧条目踢掉
    if inner.sessions.len() >= self.config.max_sessions {
        if let Some(vid) = pick_lru_victim(&inner.sessions) {
            if let Some(victim) = inner.sessions.remove(&vid) {
                spawn_disconnect(victim, self.config.shutdown_per_session_timeout);
            }
        }
    }
    inner.sessions.insert(key.clone(), arc.clone());
    Ok(arc)
}
```

**Why**：建资源（SSH 握手、TLS 握手）可能数百 ms 到数秒，**绝不能持池锁等它**。短锁 → 释放 → 慢路径 → 拿锁插入，这是经典 lookup-miss-insert 模式。

### 5. Fast-path **不要把 `is_unhealthy_now()` 当作复用条件**

```rust
// 反例
if !s.is_dead() && !s.is_unhealthy_now() {  // ❌ 错!
    return Ok(s.clone());
}

// 正例
if !s.is_dead() {                            // ✅ 只看 transport
    return Ok(s.clone());                    //   带 unhealthy 标记的原样返回
}
```

**Why（gatetime cooldown 的正确语义）**：

- `unhealthy_until` 是「上次失败后 30s 内**立即返错**，不再去打远端」的语义（autossh `AUTOSSH_GATETIME=30s` 模式）。
- 这要求 fast-path **保留** unhealthy 标记，让调用方（如 `ssh_exec` 入口）看到 `is_unhealthy_now() == true` 后立即返「cooldown 中」错误。
- 若 fast-path 把 unhealthy session 当 miss 跳过去走 slow-path 重建，**新建的 session `unhealthy_until = 0`**，调用方的 `is_unhealthy_now` 检查永远拿到 `false`，cooldown 形同虚设、30s 内会重复打远端。
- 出处：本次重构 commit `ea52f9f`（fix: 让 gatetime cooldown 真正生效）。

### 6. 把决策抽成纯函数便于测试

底层资源（`russh::client::Handle`、数据库连接）没法离线 mock 构造，但「**谁过期 / 谁该被踢**」是纯算法。把这部分独立成接受 `&[(Key, last_used_millis, age)]` 这种轻量元组的函数：

```rust
fn select_expired(
    triples: &[(Key, u64, Duration)],
    now_ms: u64,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> Vec<Key> {
    let idle_ms = idle_timeout.as_millis() as u64;
    triples.iter()
        .filter(|(_, last_used, age)| {
            now_ms.saturating_sub(*last_used) >= idle_ms || *age >= max_lifetime
        })
        .map(|(k, _, _)| k.clone())
        .collect()
}
```

**Why**：
- 完全脱开 tokio 与底层库，`#[test]`（非 `#[tokio::test]`）就能跑，比测 reaper 本身快 100×。
- 边界 case（恰好阈值 / clock skew / 空输入 / idle 与 lifetime 同时过期）一条 assertion 一条覆盖。
- reaper task body 收缩成 ~10 行编排（拿锁 → 收集 triples → 调 select_expired → 从 map remove → 释放锁 → 异步外抛 disconnect），近乎无逻辑可测。

参考：`pool.rs` 里 `select_expired` / `pick_lru_victim` 抽出后，新增 9 个针对算法的纯函数测试，几乎覆盖 reaper 全部决策路径。

---

## 反模式 / 不要这么做

### 反模式 1：reaper 持 `Arc<Mutex<Inner>>`

```rust
// ❌
let inner_for_reaper = inner.clone(); // Arc clone
tokio::spawn(async move {
    loop {
        // ... 用 inner_for_reaper ...
    }
});
```

后果：pool drop 后 strong_count 不归零，inner 永不释放，reaper task 永不退出（除非用别的 channel 通知它），进程退出可能 hang。

### 反模式 2：`shutdown()` 持锁状态 await disconnect

```rust
// ❌
pub async fn shutdown(&self) {
    let mut inner = self.inner.lock().await;
    for (_, s) in inner.sessions.drain() {
        s.disconnect().await; // ← 持锁 await,期间任何 acquire/reaper 都会卡死
    }
}
```

后果：disconnect 期间 acquire 阻塞。远端慢 → acquire 慢 → 调用方超时 → 雪崩。`drain` 后立刻 drop 锁、然后 join_all。

### 反模式 3：fast-path 校验 unhealthy

```rust
// ❌
if !s.is_dead() && !s.is_unhealthy_now() {
    return Ok(s.clone());
}
```

后果：cooldown 失效，详见「关键决策 5」。

### 反模式 4：reaper 自旋 `loop { interval.tick(); ... }` 没有退出条件

```rust
// ❌
tokio::spawn(async move {
    let mut t = tokio::time::interval(tick);
    loop { t.tick().await; do_work(); }
});
```

只靠外部 `JoinHandle::abort()` 退出。`abort()` 在 await point 才生效，若 do_work 内有未 cancel-safe 的临界区，可能在 abort 时留下半完成状态。**用 `Weak::upgrade()` 自然退出 + Drop 兜底 abort 是更鲁棒的组合**。

---

## 真实落地点

本仓 `src-tauri/mt-sidecars/src/pool.rs` 是本骨架的参考实现。文件内每个关键决策都有对应注释引回这里能用得上的判断点：

- `spawn_reaper`：Weak upgrade 退出循环（决策 1）。
- `Drop for SshPool` + `reaper: std::sync::Mutex<...>`（决策 2）。
- `SshPool::shutdown`：abort → drain → drop lock → join_all（决策 3）。
- `SshPool::acquire`：fast-path 短锁 + 不查 unhealthy（决策 4 + 5）。
- `select_expired` / `pick_lru_victim` + 测试：纯函数抽决策（决策 6）。

新写同类池子（sftp / http2 / db）时，**先复制这套骨架，再换底层 handle 类型 + disconnect API**，剩下的几乎是机械替换。

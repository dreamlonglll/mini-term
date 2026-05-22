# Research: russh vs ssh2 for mt-ssh-mcp 持久会话池

- **Query**: 对比 Rust SSH client 库 `russh` 与 `ssh2`,用于把 `spawn ssh user@host <cmd>` 改成在进程内常驻一个 `Session` pool。
- **Scope**: 外部(crates.io / docs.rs / GitHub / OSV);仅查阅,不动主仓库代码。
- **Date**: 2026-05-22 (Asia/Shanghai)
- **数据快照**: russh 0.61.0 (2026-05-20 发布);ssh2 0.9.5 (2025-02-01 发布,内含 libssh2 1.11.1 2024-10-16)。

---

## 1. TL;DR

**选 `russh`(0.61+)。** 决定性原因:本项目已是 tokio + rmcp async 栈,而 `russh` 是原生 tokio async + 纯 Rust,**ProxyJump 几乎是「免费」**(`Channel::into_stream()` 返回 `AsyncRead + AsyncWrite`,直接喂给 `client::connect_stream` 即可嵌套一个新 Session,完全在 Rust 类型系统内组合)。`ssh2` 包装 `libssh2`(C 库)只接受裸 `RawFd`,做 ProxyJump 必须自己撑 socketpair + 后台 polling 线程(详见 ssh2-rs #167 维护者 wez 的回答),且整段会卡进 `spawn_blocking`,与已有 async runtime 阻抗失配。维护活性也压倒性偏向 russh(0.61 五月发布,周更;ssh2 0.9.5 已停在 2025-02,关键 issue 如 ECDSA-384 不支持、Dual Authentication 没人接)。Windows 静态构建上两者都 OK:russh 全 Rust 无 C 依赖,ssh2 在 MSVC 上默认走 WinCNG 也不需要 OpenSSL,差距不大。

唯一让 ssh2 略占便宜的点:**API 同步直白**(`sess.userauth_password(...).unwrap(); let mut ch = sess.channel_session()?; ch.exec("ls")?; ch.read_to_string(...)?;`),而 russh 要写一个 `client::Handler` trait 实现 + 跑 `ChannelMsg` 事件循环。但本项目场景下池里一条连接一次只接一个 agent 调用,事件循环就 50 行,值得为后面的 ProxyJump、agent forward、未来的 SFTP 多 channel 一次性补好。

---

## 2. Side-by-side table

| 维度 | russh (warp-tech/russh) | ssh2 (alexcrichton/ssh2-rs) |
|---|---|---|
| 实现语言 | 纯 Rust(fork 自 thrussh,Pierre-Étienne Meunier) | Rust 绑定 + 内嵌 libssh2 C 库 |
| 运行时模型 | async (tokio),trait 异步方法 | sync,阻塞 I/O(`set_blocking(false)` 可手动非阻塞但 API 仍是 `Result`) |
| 当前版本 / 发布日 | **0.61.0 / 2026-05-20** | **0.9.5 / 2025-02-01** |
| 0.x → 1.0 状态 | 0.x 但 API 数月一动,小破坏改版常态 | 0.9 长期稳定,2 年只发了 0.9.5 一个小补丁 |
| crates.io 全量下载 | ~3.5M | ~6.4M(历史累积更高) |
| crates.io 近 90 天下载 | ~1.27M(增速更高) | ~780k(下滑) |
| GitHub stars | ~1,715 | ~559 |
| 开放 issue / 最新一条 | 68 个 / **2026-05-21**(一天前) | 46 个 / 2026-01-06(最近一条);多个 2024 年关键 issue 无人接(#346 Dual Auth、#343 ECDSA-384) |
| 最近 commit | 2026-04 起持续小步快跑 | 2026-04-24 合并了一个 io::Error 解析 PR,主线开发已停滞 |
| Tokio / 异步集成 | 原生 | 需 `spawn_blocking` 或第三方 `async-ssh2-lite`(0.5.0 / 2024-07 也已停滞) |
| 密码认证 | `authenticate_password(user, pass)` | `userauth_password(user, pass)` |
| 私钥认证 | `authenticate_publickey(user, PrivateKeyWithHashAlg::new(...))`,内部 `russh::keys::load_secret_key(path, Some(passphrase))` 直接读 OpenSSH 格式 | `userauth_pubkey_file(user, pubkey, privkey, passphrase)` 或 `userauth_pubkey_memory(...)` |
| 加密私钥(passphrase) | `load_secret_key(path, Some(pw))` 显式参数 | `userauth_pubkey_file` 接受 passphrase 参数 |
| keyboard-interactive | `authenticate_keyboard_interactive_start` + `authenticate_keyboard_interactive_respond`(分两步,可以塞 prompt 列表) | `userauth_keyboard_interactive(user, &mut Prompter)`,需实现 `KeyboardInteractivePrompt` trait |
| ssh-agent | `russh_keys::agent::client::AgentClient`,**Windows Pageant 原生支持** ✨ | `Session::agent()` → `Agent::userauth(...)`,**Windows 走 Pageant 由 libssh2 1.11+ 提供**(WinCNG 后端) |
| ProxyJump / 跳板机 | **原生路径**:`bastion.channel_open_direct_tcpip(target_host, port, …)` → `channel.into_stream()` 得 `AsyncRead+AsyncWrite` → `client::connect_stream(config, stream, handler)` → 拿到内层 Session,然后正常 auth+exec | **没有原生 API**:libssh2 需要 raw fd,channel 不是 fd;社区方案是 socketpair + 后台线程双向 forward(见 ssh2-rs #167 wez 的官方回复)或用第三方 `async-ssh2-lite` `proxy_jump` demo;**未维护** |
| Channel API(exec) | `Handle::channel_open_session() -> Channel<Msg>`,`channel.exec(true, cmd).await?`,然后 `loop { match channel.wait().await { ChannelMsg::Data{data}=>..., ChannelMsg::ExitStatus{exit_status}=>... } }` | `Session::channel_session() -> Channel`,`channel.exec(cmd)?`,然后 `channel.read_to_string(&mut buf)?; channel.wait_close()?; channel.exit_status()?` |
| 多 channel 并发 | 在一个 Session 内 `channel_open_session` 可多次,每个 channel 独立 future,内部走 SSH multiplexing | 同一个 Session 上多 channel 共享 libssh2 内部状态,**不能多线程并发读写**(wez 在 #84 明确提醒);串行 OK |
| `known_hosts` | `russh::keys::HashedHostname` + `Handler::check_server_key` 回调;`russh-config` 子 crate 可读 ssh_config | `Session::known_hosts()` → `KnownHosts::read_file/write_file/check/add` |
| 主机密钥校验时机 | 在 `Client::Handler::check_server_key` 回调里,**握手期间**同步决定 | `sess.host_key()` / `sess.host_key_hash()`,在 handshake 之后手动 check;漏检容易引入 MITM |
| Windows 构建依赖 | 纯 Rust + `aws-lc-rs` 默认(可换 `ring`);零 C 依赖 | C 编译:`cc` + `vcpkg`;**MSVC 默认走 libssh2 的 WinCNG 后端,不连 OpenSSL**;`openssl-on-win32` feature 才需要 OpenSSL;`vendored-openssl` feature 静态化 |
| 加密后端选择 | `aws-lc-rs`(默认,FIPS 友好) 或 `ring`;两者都不能同时关 | Windows MSVC:WinCNG;Unix:OpenSSL(动态或 vendored 静态);可选 `zlib-ng-compat` |
| 内嵌 libssh2 版本 | 不适用 | 1.11.1(2024-10),vendored 在 ssh2-rs 仓库 submodule;升级要等 ssh2 crate 出新版 |
| 安全公告(已修复) | 6 条 OSV(全在 ≤0.60.3 修完):pre-auth DoS、CryptoVec、ChaCha20-Poly1305 Terrapin、unsafe DH 等;**响应活跃** | crate 本身 0 条;libssh2 上游 9 条 OSS-Fuzz 报告(2022-2025),NULL deref / heap overflow / double-free 居多 |
| 最高级风险 | DoS 类居多,被动攻击难触发(KEXINIT/keyboard-interactive 路径) | C 内存安全:libssh2 历史上多次 heap overflow / double-free;触发面是处理恶意服务器返包时 |
| 生态 / 上层封装 | `async-ssh2-tokio`(高层 client,~57k 月下载,2026-01 更新);`russh-sftp` 同仓库维护 | `async-ssh2-lite`(0.5.0,2024-07 停滞);`thrussh` 已弃 |
| 文档 | docs.rs 完整;5 个官方 example(client_exec_simple / client_exec_interactive / client_open_direct_tcpip / sftp_client / echoserver) | docs.rs lib.rs 顶部有 5 段 inline 教程;无独立 examples 目录 |
| MSRV | 1.85(2026-Q1) | 未声明,实际可在更老 rustc 跑 |

---

## 3. Approach A: russh

### 3.1 会话池伪代码草图(基于本项目类型 `mt_core::SshConnection`)

```rust
// 伪代码,**不写进主仓**,只展示形态。
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use russh::*;
use russh::client::{self, Handler, Config, Msg, Session as ClientSession};
use russh::keys::*;

pub type ConnId = String;
pub type SessionMap = Arc<Mutex<HashMap<ConnId, Arc<Mutex<Pooled>>>>>;

/// 池里的一项:握好手 + 认完证之后挂起的 client handle。
struct Pooled {
    handle: client::Handle<MtClient>,
    last_used: std::time::Instant,
}

/// 主机密钥校验策略由调用方注入,先用 accept-new 等价语义占位。
struct MtClient {
    // 这里挂 known_hosts 路径、is_first_seen flag 等。
}

impl Handler for MtClient {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        server_pubkey: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO: 比对 known_hosts;首见接受、变更拒绝(StrictHostKeyChecking=accept-new 语义)。
        Ok(true)
    }
}

/// 按 connection 拿(必要时 lazy 连接)。
async fn get_or_connect(
    pool: &SessionMap,
    conn: &mt_core::SshConnection,
) -> anyhow::Result<Arc<Mutex<Pooled>>> {
    let id = conn.id.clone();
    let mut map = pool.lock().await;
    if let Some(p) = map.get(&id) {
        // TODO: ping channel 检测是否还活;不活就移除重连。
        return Ok(p.clone());
    }
    drop(map); // 解锁后慢动作再上锁
    let pooled = build_session(conn).await?;
    let arc = Arc::new(Mutex::new(pooled));
    pool.lock().await.insert(id, arc.clone());
    Ok(arc)
}

async fn build_session(conn: &mt_core::SshConnection) -> anyhow::Result<Pooled> {
    let cfg = Arc::new(Config { inactivity_timeout: Some(std::time::Duration::from_secs(60)), ..Default::default() });
    let handler = MtClient {};

    // —— ProxyJump 关键路径 ——
    let mut handle = if let Some(jump) = conn.proxy_jump.as_deref().filter(|s| !s.is_empty()) {
        // "user@bastion[:port]" → 拆开成 SshConnection-like;此处省略 parser。
        let (b_user, b_host, b_port) = parse_jump(jump)?;
        let bastion_cfg = cfg.clone();
        let bastion_handle = client::connect(bastion_cfg, (b_host.as_str(), b_port), MtClient {}).await?;
        // 跳板机自身的 auth:复用主连接的 identity_file / agent / password,按需选其一。
        auth_handle(&mut bastion_handle, b_user.as_str(), conn).await?;
        // 在 bastion 上开 direct-tcpip 到目标主机的 22,拿 AsyncRead+AsyncWrite。
        let channel = bastion_handle
            .channel_open_direct_tcpip(conn.host.as_str(), conn.port as u32, "127.0.0.1", 0)
            .await?;
        let stream = channel.into_stream();
        client::connect_stream(cfg.clone(), stream, handler).await?
    } else {
        client::connect(cfg.clone(), (conn.host.as_str(), conn.port), handler).await?
    };

    auth_handle(&mut handle, &conn.user, conn).await?;
    Ok(Pooled { handle, last_used: std::time::Instant::now() })
}

/// 一条 exec 调用:开一个 channel,执行,收集 stdout/stderr/exit,关闭。
pub async fn exec(
    pool: &SessionMap,
    conn: &mt_core::SshConnection,
    command: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<SshExecResult> {
    let pooled = get_or_connect(pool, conn).await?;
    let mut p = pooled.lock().await;
    let mut ch = p.handle.channel_open_session().await?;
    ch.exec(true, command).await?;

    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut exit: Option<i32> = None;
    let mut timed_out = false;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => { timed_out = true; break; }
            msg = ch.wait() => {
                let Some(msg) = msg else { break };
                match msg {
                    ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext } if ext == 1 => stderr.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status as i32),
                    ChannelMsg::Close | ChannelMsg::Eof => continue, // 不要立刻退出,可能还有 ExitStatus 待收
                    _ => {}
                }
            }
        }
    }
    let _ = ch.close().await;
    p.last_used = std::time::Instant::now();

    Ok(SshExecResult {
        stdout: cap_to_utf8(stdout),
        stderr: cap_to_utf8(stderr),
        exit_code: exit,
        truncated: false, // 实际按 OUTPUT_CAP_BYTES 截
        timed_out,
    })
}
```

### 3.2 优点(对本项目)

1. **ProxyJump 是组合,不是 hack**:`channel.into_stream()` → `client::connect_stream(stream)`,新 Session 是同一类型,可以再嵌套(多级跳板);串行、并发、超时全靠 tokio 现有原语,跟 rmcp 套得很自然。
2. **零 C 依赖,Tauri sidecar 打包简单**:全 Rust + `aws-lc-rs`(已内置 prebuilt 或 vendored 编译),`cargo build --target x86_64-pc-windows-msvc` 不需要 vcpkg / OpenSSL / cmake 介入。
3. **类型安全的密钥处理**:`russh::keys::load_secret_key(path, Some(pass))` 直接对 OpenSSH 格式生效;passphrase / Ed25519 / RSA / ECDSA / PPK 全在一条路径。已有的 `mt_core::ssh_key::sanitize_key_permissions` 临时副本逻辑,可以从「写文件给 ssh 进程读」简化成「直接 load 到内存」,反而少一次 IO。
4. **维护活性高**:0.61 五月发布,每周都有 commit / merge;最近 PR 包含 KEXINIT 协议修复、DSA 签名长度修复(说明 0.60.x 有过坑,但修得也快)。
5. **Pageant + ssh-agent on Windows**:README 明确列出 Pageant 支持,且 0.60 加了 SSH_AUTH_SOCK get/set 支持(commit 5219ffba)。本项目想接 Windows OpenSSH 自带的 ssh-agent.exe(命名管道 `\\.\pipe\openssh-ssh-agent`)路径已通。
6. **已修复 6 条安全公告,响应时间短**:最近一条 GHSA-f5v4-2wr6-hqmg(keyboard-interactive pre-auth DoS)在 0.60.1 修掉,显示团队在跟 advisory。
7. **下游 wrapper 选项**:`async-ssh2-tokio`(同生态,2026-01 更新)如果嫌底层 API 啰嗦,可以挑这层做候选,但本任务建议直接吃 russh 原生 API,避免再多一层耦合。

### 3.3 缺点 / 注意

1. **API 啰嗦**:`Handler` trait + `ChannelMsg` 事件 loop,比 ssh2 的 `read_to_string` 直觉差,初次写要 50-80 行胶水。
2. **MSRV 1.85**:本仓库 `mt-core` 当前 `edition = "2021"`,需要确认 toolchain;rustc 1.85 在 2026-Q1 已 stable,正常 CI 没问题。
3. **加密后端必须二选一**:不能两个都 disable;默认 `aws-lc-rs` 在某些受限平台(比如 Android `aarch64-linux-android`)需要额外处理;Windows/macOS/Linux x86_64 没事。
4. **API 在 0.60 → 0.61 之间有 breaking change**:`load_secret_key` 签名、`PrivateKeyWithHashAlg`、Handler 异步签名都改过;开发期要锁版本(`russh = "=0.61"`),后续小步升级。
5. **`check_server_key` 是 Handler 状态机里的回调**:要拿外部 known_hosts 文件路径就得把它塞进 Handler struct,跨多个连接时每个 connection 一个 Handler 实例更清晰。
6. **DoS 公告显示协议层模糊测试还在揭新坑**:0.60.x 集中爆了几个,提示一定要跟着 cargo audit 升级;比 ssh2 风险点不同——前者协议解析坑、后者 C 内存坑。

---

## 4. Approach B: ssh2

### 4.1 会话池伪代码草图(同步 + spawn_blocking)

```rust
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::Arc;
use parking_lot::Mutex;
use ssh2::Session;

pub type SessionMap = Arc<Mutex<HashMap<String, Arc<Mutex<Pooled>>>>>;

struct Pooled {
    session: Session,           // 拥有 underlying TcpStream
    last_used: std::time::Instant,
}

fn build_session_blocking(conn: &mt_core::SshConnection) -> anyhow::Result<Pooled> {
    // —— ProxyJump 路径(libssh2 不接 channel-as-fd,需要 socketpair + 双向 pump) ——
    let tcp: Box<dyn ReadWrite> = if let Some(jump) = conn.proxy_jump.as_deref() {
        let (b_user, b_host, b_port) = parse_jump(jump)?;
        let b_tcp = TcpStream::connect((b_host.as_str(), b_port))?;
        let mut bastion = Session::new()?;
        bastion.set_tcp_stream(b_tcp);
        bastion.handshake()?;
        auth_session(&mut bastion, &b_user, conn)?;
        let mut bastion_ch = bastion.channel_direct_tcpip(&conn.host, conn.port, None)?;

        // 关键 hack:libssh2 需要内层 session 拿到一个 RawFd / std::net::TcpStream-like。
        // channel 既不是 fd 也不是 TcpStream。社区方案:
        //   1. 建 socketpair(filedescriptor crate 或 mio_uds_windows / 自己包 named pipe);
        //   2. 一端给内层 Session.set_tcp_stream;
        //   3. 起两个线程,在 bastion_ch ↔ socketpair 之间双向 copy_bidirectional;
        //   4. bastion session 切 non-blocking,主线程 poll 它的 fd。
        // 见 ssh2-rs #84 wez 回复 + ssh2-rs #167。Windows 上 socketpair 还得用 ws2_32
        // 的 loopback-AF_INET 模拟,代码量~150 行,且要管两个线程的生命周期 / 错误传播。
        //
        // 替代:async-ssh2-lite 的 proxy_jump 例子(但 crate 已 2024-07 后停滞,
        // 不在本团队可控维护范围)。
        Box::new(socketpair_proxy(bastion, bastion_ch)?)
    } else {
        Box::new(TcpStream::connect((conn.host.as_str(), conn.port))?)
    };

    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);          // ⚠️ set_tcp_stream 接 Into<std::net::TcpStream> + RawFd
    sess.handshake()?;
    auth_session(&mut sess, &conn.user, conn)?;
    // host key check 必须自己写:sess.host_key() 拿到 raw key,再对 known_hosts 校验。
    Ok(Pooled { session: sess, last_used: std::time::Instant::now() })
}

fn auth_session(sess: &mut Session, user: &str, conn: &mt_core::SshConnection) -> anyhow::Result<()> {
    if let Some(key) = conn.identity_file.as_deref() {
        sess.userauth_pubkey_file(user, None, std::path::Path::new(key), conn.password.as_deref())?;
    } else if let Some(pw) = conn.password.as_deref() {
        // password 失败 fallback 到 keyboard-interactive
        if sess.userauth_password(user, pw).is_err() {
            struct P<'a>(&'a str);
            impl<'a> ssh2::KeyboardInteractivePrompt for P<'a> {
                fn prompt<'b>(&mut self, _u: &str, _instr: &str, prompts: &[ssh2::Prompt<'b>]) -> Vec<String> {
                    prompts.iter().map(|_| self.0.to_string()).collect()
                }
            }
            sess.userauth_keyboard_interactive(user, &mut P(pw))?;
        }
    }
    if !sess.authenticated() {
        anyhow::bail!("auth failed");
    }
    Ok(())
}

pub async fn exec(pool: SessionMap, conn: mt_core::SshConnection, command: String) -> anyhow::Result<SshExecResult> {
    // 同步 API → 进 spawn_blocking
    tokio::task::spawn_blocking(move || -> anyhow::Result<SshExecResult> {
        let pooled_arc = {
            let mut map = pool.lock();
            if let Some(p) = map.get(&conn.id) { p.clone() } else {
                let p = Arc::new(Mutex::new(build_session_blocking(&conn)?));
                map.insert(conn.id.clone(), p.clone());
                p
            }
        };
        let mut p = pooled_arc.lock();
        let mut ch = p.session.channel_session()?;
        ch.exec(&command)?;

        let mut stdout = String::new();
        ch.read_to_string(&mut stdout)?;
        let mut stderr = String::new();
        ch.stderr().read_to_string(&mut stderr)?;
        ch.wait_close()?;
        let exit = ch.exit_status().ok();
        p.last_used = std::time::Instant::now();
        Ok(SshExecResult { stdout, stderr, exit_code: exit, truncated: false, timed_out: false })
    }).await?
}
```

### 4.2 优点

1. **API 直觉**:`ch.exec(...)?; ch.read_to_string(...)?; ch.exit_status()?` —— 6 行就跑通,易上手。
2. **同步阻塞模型容错好理解**:任何错误立刻 Result return,不存在 channel 事件丢失、半关闭状态机模糊点。
3. **生态老资历**:Cargo.lock 里很多 Rust 大项目(包括早期 cargo 自己)用过 libssh2 / ssh2,踩过的坑文档化好。
4. **Windows 静态构建意外简单**:MSVC 默认走 libssh2 的 **WinCNG**(`LIBSSH2_WINCNG`),链 `bcrypt.lib crypt32.lib user32.lib ntdll.lib`,**完全不需要 OpenSSL**;`libssh2-sys/build.rs:101-128` 行明示此分支。这是之前 task 中容易高估的复杂度。
5. **0 条 OSV crate-level 公告**:Rust 封装本身没爆过,但下面 libssh2 C 库自己有(见缺点)。

### 4.3 缺点(对本项目)

1. **ProxyJump 是硬伤**:libssh2 只接 raw fd / TcpStream,channel 不能当 fd。要么不支持 -J(回退到 spawn ssh,违背重构本意),要么写 ~150 行 socketpair + 双向 pump 线程并自己管错误/超时/生命周期。Windows 上 socketpair 还得 AF_INET loopback 模拟。**这个坑在 ssh2-rs #167 自 2020 年开到现在(2026)没解决,可视为「事实上不会被原作者支持」**。
2. **维护停滞**:0.9.5 已经 1 年多没更新,关键开放 issue:
   - #346 (2025-01) Dual Authentication(publickey,password)不支持 — 部分企业服务器强制需要;
   - #343 (2024-12) ECDSA-384 host key 不支持(kex);
   - #341 (2024-12) Timeout 行为不对;
   - #325 (2024-08) 多 channel 并发问 socket。
3. **async 适配丑陋**:整段塞 `spawn_blocking`,池里多个连接想并发服务多个 agent 请求只能堆 blocking 线程,与 rmcp tokio 模型冲突;或者依赖已停滞的 `async-ssh2-lite`(2024-07 后无更新)。
4. **libssh2 C 内存安全**:OSV 显示 2022-2025 间有 9 条 OSS-Fuzz 报告(heap-buffer-overflow / double-free / null deref)。bundled 1.11.1 是 2024-10 发布的最新稳定,但**ssh2-rs 何时升 submodule 取决于 alexcrichton 是否还活跃**(从近 1 年提交节奏看,响应窗口可能数月)。
5. **Session 是 `!Send` 的?——其实是 Send,但 `Channel` 借 Session 寿命**:`Channel<'sess>` 的生命周期绑死在 Session 上,如果想把 channel 移交别的 task 跑超时,需要 `Arc<Mutex<Session>>` + 在 lock 内创建/销毁 channel,代码丑且 Mutex 颗粒度大。
6. **keyboard-interactive callback 接 trait**:需要实现 `KeyboardInteractivePrompt`,而 prompt 又借 `&[Prompt<'a>]`,生命周期写起来比 russh 的 start/respond 分两步 API 啰嗦(本项目把 password 当 keyboard-interactive 答案的统一回退路径要这样写)。
7. **host key 校验默认不做**:ssh2-rs `handshake()` 不会自动比对 known_hosts,得手动 `sess.host_key()` + `sess.known_hosts().check_port(...)`;漏写一行就是 MITM。russh 的 `Handler::check_server_key` 是必须实现的回调,**类型系统强制开发者面对这事**。

---

## 5. Approach C: hybrid(russh 主路径 + spawn ssh 仅给 ProxyJump)

不推荐。理由:

1. **本任务的初衷就是消灭 spawn ssh**——为了避免 ssh.exe 进程冷启动开销、避免 PTY 自动填密码的脆弱性、避免不同平台 ssh.exe 行为差异。如果 -J 仍走 spawn,等于把最复杂的一类连接留在最易出问题的路径上,价值减半。
2. **代码路径双倍**:auth / 输出截断 / 审计日志 / 超时,都要在「russh path」和「spawn path」各写一遍,后续每次改动都得双改。
3. **russh 的 ProxyJump 路径并不复杂**:就是「连 bastion → 开 direct-tcpip → connect_stream」,核心 30 行,且 100% async 兼容。
4. **唯一可能合理的「混合」是降级**:russh 连不上时(libssh 协议小众算法不支持的极端情况)fallback 到 spawn ssh —— 但这是错误恢复策略,不是常规架构。可以放到后续 P3 任务。

---

## 6. Risks / unknowns(需要 spike 才能定的事)

1. **Windows OpenSSH 自带 ssh-agent.exe 互通**
   - 它使用命名管道 `\\.\pipe\openssh-ssh-agent`,不是 unix socket。
   - russh 的 README 写了 Pageant 支持(Putty 的 agent,共享 mmap),但 OpenSSH 命名管道是另一个协议。
   - **需要 spike**:在 Windows 11 上启动 ssh-agent.exe,加入一个 ed25519 key,然后用 russh `russh_keys::agent` 试一次;若不行,看 `async-ssh2-tokio` 或 PR(#355 set/get auth_sock 那个)是否支持 win-named-pipe。
   - 风险等级:中。若不支持,可以先不支持 agent,只支持 password/keyfile,后续再补。

2. **Windows OpenSSH 私钥默认权限保护**
   - 当前 `mt_core::ssh_key::sanitize_key_permissions` 写了一份收紧权限的副本喂给 ssh.exe。
   - 如果用 russh 直接 `load_secret_key(path, ...)` 读内存,**就不再需要副本/权限检查**(直接读 → 字符串解析 → 转 Key,与文件 mode 无关)。
   - **需要 spike**:确认这次重构后,旧的 `sanitize_key_permissions` 路径整段可以删,不会被前端别处依赖。

3. **`channel_open_direct_tcpip` 在 bastion 上需要服务端允许 AllowTcpForwarding**
   - 与 spawn ssh -J 行为一致(`ssh -J` 内部也是发起 direct-tcpip)。
   - 如果用户的 bastion `AllowTcpForwarding no`,两种实现都得报错;russh 错误是 `russh::Error::ChannelOpenFailure`,错误码 `OPEN_ADMINISTRATIVELY_PROHIBITED`,文案要友好转一下。

4. **多级 ProxyJump(`-J h1,h2,h3`)**
   - 本项目 `SshConnection::proxy_jump` 是一个 `Option<String>`,**当前 spawn ssh 用 `-J <jump>` 把它原样传给 OpenSSH**;`-J` 可以接逗号分隔的多跳。
   - russh 重构后,如果想支持多跳,需要把 jump string 用 `,` 拆开,然后递归 connect_stream(每一跳的 channel 给下一跳作 stream)。
   - **决定**:MVP 阶段建议**只支持单跳**(`user@host`),多跳报「unsupported, fall back to spawn ssh」错误;后续 P3 再做。先确认实际用户里有多少多跳场景。

5. **ssh2 socketpair-proxy 在 Windows 上**
   - 如果改主意走 ssh2,这步在 Windows 必须用 AF_INET-loopback 模拟 socketpair(Windows 没有 unix domain socketpair API)。
   - 已有的 `wez/filedescriptor` crate 0.7+ 提供跨平台 socketpair(Windows 走 loopback),但**多一个 C-ish 依赖**,且每个 SSH 连接吃两个 fd + 两个后台线程。
   - 在 1000 个连接的池里就是 2000 线程,不可接受。所以 ssh2 路径对持久会话池本质不友好。

6. **Session keepalive**
   - russh `Config::inactivity_timeout` + `Config::keepalive_interval` 控制。
   - ssh2 `Session::set_keepalive(want_reply, interval)` 内部需要周期性调用 `Session::keepalive_send`(由用户驱动,libssh2 不自己后台跑)。
   - **决定**:无论选哪个,池里应该跑一个 tick 协程 / 线程,每 30s 给空闲连接发 keepalive,失败的剔出池。

7. **断线检测 / 自愈**
   - 池里的 Session 可能因服务器空闲超时被踢。
   - 第一次 exec 失败时,应该清掉缓存重建 Session 重试一次(只重试一次,避免认证暴力)。
   - 与连接复用语义无关的纯实现细节,两个库都能做,但 russh 错误类型枚举更细(`russh::Error::Disconnect` 等)更容易判断。

8. **crate audit 接入**
   - `cargo audit` 当前没在 CI 跑(从 `.github/workflows/release.yml` 看)。建议本任务里顺手加上,确保 russh 的 advisory 一出就被发现。

---

## Findings — files referenced

### External References

| 资源 | 重要性 / 用途 |
|---|---|
| https://crates.io/crates/russh ([0.61.0, 2026-05-20](https://crates.io/api/v1/crates/russh)) | 版本与下载数据源 |
| https://github.com/warp-tech/russh | 主开发库(原 `Eugeny/russh` 已转交 warp-tech 组织,两个 URL 都跳同一仓库) |
| https://github.com/warp-tech/russh/blob/main/russh/examples/client_exec_simple.rs | exec 调用模板 |
| https://github.com/warp-tech/russh/blob/main/russh/examples/client_open_direct_tcpip.rs | ProxyJump 的核心 building block:`channel_open_direct_tcpip` + `into_stream` |
| https://docs.rs/russh/0.61.0/russh/client/index.html | `client::connect`、`client::connect_stream<S: AsyncRead+AsyncWrite>` 签名 |
| https://docs.rs/russh/0.61.0/russh/client/struct.Handle.html | `authenticate_password / publickey / keyboard_interactive_start/respond / none / channel_open_session / channel_open_direct_tcpip` 列表 |
| https://docs.rs/russh/0.61.0/russh/struct.Channel.html | `Channel::into_stream() -> ChannelStream (AsyncRead+AsyncWrite)`、`make_reader / make_writer` |
| https://docs.rs/russh/0.61.0/russh/keys/agent/index.html | ssh-agent + Pageant 支持(Windows) |
| https://crates.io/crates/ssh2 ([0.9.5, 2025-02-01](https://crates.io/api/v1/crates/ssh2)) | 版本与下载数据源 |
| https://github.com/alexcrichton/ssh2-rs | 主仓库,维护活性参考 |
| https://github.com/alexcrichton/ssh2-rs/blob/master/libssh2-sys/build.rs | Windows MSVC 默认走 WinCNG 不需要 OpenSSL 的证据(行 101-128) |
| https://github.com/alexcrichton/ssh2-rs/issues/167 | 2020 开到现在(2026)未解的 bastion / ProxyJump 缺失;wez 给出 socketpair 方案 |
| https://github.com/alexcrichton/ssh2-rs/issues/84 | 关于 bastion / ssh-agent forwarding 的早期讨论 |
| https://docs.rs/ssh2/0.9.5/ssh2/struct.Session.html | `channel_session / channel_direct_tcpip / userauth_password / userauth_pubkey / userauth_keyboard_interactive / userauth_agent / known_hosts / set_tcp_stream / set_timeout / handshake / disconnect` |
| https://github.com/alexcrichton/ssh2-rs/blob/master/Cargo.toml | feature flags(只两个:`vendored-openssl`、`openssl-on-win32`) |
| https://api.osv.dev/v1/query (russh) | 6 条 GHSA,全部已在 ≤0.60.3 修复 |
| https://api.osv.dev/v1/query (ssh2 / libssh2-sys) | 0 条 crate-level;libssh2 C 库另有 9 条 OSS-Fuzz |
| https://crates.io/crates/async-ssh2-tokio (0.12.2, 2026-01) | russh 高层包装,可选 |
| https://crates.io/crates/async-ssh2-lite (0.5.0, 2024-07) | ssh2 的 async 包装,**已停滞** |

### Project files (current code touched by this decision)

| 文件 | 描述 |
|---|---|
| `src-tauri/mt-sidecars/src/bin/mt-ssh-mcp.rs` | 当前 spawn `ssh` 子进程的 `ssh_exec` 实现,需重构成 russh 池 |
| `src-tauri/mt-core/src/ssh_connection.rs` | `SshConnection`:`password / identity_file / proxy_jump` 字段,russh path 直接用 |
| `src-tauri/mt-core/src/ssh_key.rs` | 当前为 ssh.exe 收紧权限做的临时副本;russh path 不再需要,可在重构里移除 |
| `src-tauri/mt-core/src/ssh_prompt.rs` | PTY autofill 密码的 prompt 扫描;russh path 走 SSH 协议层直接 `authenticate_password`,这段对 mt-ssh-mcp 不再用 |
| `src-tauri/mt-core/src/config_reader.rs` | `read_ssh_connections_for_project` 按项目过滤,与 SSH 库选择无关,保持原状 |

### Related Specs

| 文件 | 描述 |
|---|---|
| `.trellis/spec/backend/index.md` | mt-core 共享 crate 约定 |
| `.trellis/spec/backend/agent-config-injection.md` | 与 SSH 库无关,但 mt-ssh-mcp 启停一致性写在这里 |

## Caveats / Not Found

1. **没找到** russh 官方的 ProxyJump example。结论是基于 `client_open_direct_tcpip.rs` example + `Channel::into_stream` 文档 + `client::connect_stream` 签名推出来的,理论上可行;**第一步 spike 建议**先在一个 throwaway crate 里跑一次「russh 通过 bastion 拿到目标 host 的 `uname -a`」,确认无暗坑。
2. **Windows OpenSSH ssh-agent.exe(命名管道)与 russh 互通**未在文档中明确证实,只看到 PR #355 加了 SSH_AUTH_SOCK get/set。第二步 spike 验证。
3. **没量化**两种实现下的 startup latency:理论上 russh 持久 Session 复用应该比 spawn ssh 每次冷启动快 5-10x,但准确数字要等 prototype 跑出来。
4. **GitHub API 受未认证速率限制**,本研究的 commits / contents 接口数据可能截断;关键事实(crate 版本、ProxyJump issue 内容、build.rs Windows 分支)均由静态文件 raw URL 直接核实。

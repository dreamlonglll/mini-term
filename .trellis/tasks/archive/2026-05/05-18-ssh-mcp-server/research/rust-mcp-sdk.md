# Research: 用 Rust 实现 stdio MCP Server（rmcp 选型）

- **Query**: 用 Rust 实现一个 stdio 传输的 MCP Server sidecar，暴露 `ssh_list_connections` / `ssh_exec` 工具；调研 `rmcp` crate 的成熟度、最小骨架、依赖体积、替代方案、已知坑。
- **Scope**: external（库选型调研）
- **Date**: 2026-05-18
- **数据来源**: crates.io API、docs.rs、GitHub `modelcontextprotocol/rust-sdk` 仓库（源码 + Cargo.toml + CHANGELOG）

---

## 一句话结论

**用官方 `rmcp` crate（当前 1.7.0）。它已成熟、活跃维护、API 自 1.0 起稳定，是事实标准（10M+ 下载，竞品低一个数量级）。代价是强制依赖 tokio + 约 30 个依赖、edition 2024（需 Rust ≥ 1.85）——对一个 sidecar 完全可接受，不必为了"轻"去手写 JSON-RPC。**

---

## 1. rmcp 版本与成熟度结论

### 关键事实

| 项 | 值 |
|---|---|
| crate 名 | `rmcp`（"Rust SDK for Model Context Protocol"，**官方** SDK，仓库 `modelcontextprotocol/rust-sdk`）|
| 最新版本 | **1.7.0**，发布于 **2026-05-13**（距今 5 天）|
| 首次发布 | 2025-03-16；**1.0.0 发布于 2026-03-03** |
| 发布节奏 | 每月一个 minor，1.0 以来 9 个版本（1.0→1.7），无 yank |
| 累计下载 | **10,073,576**；近 90 天 6,381,876 |
| GitHub | **3,430 star**，521 fork，**仅 25 个 open issue**，`pushed_at` = 2026-05-18（今天）|
| License | Apache-2.0 |
| edition | **2024**（workspace `edition = "2024"`），CHANGELOG #797 "update Rust toolchain to 1.92" |
| docs.rs | https://docs.rs/rmcp |

### 成熟度判断

- **成熟、可放心用**。10M 下载、官方背书、25 个 open issue（相对 3.4k star 很低）、每天都有提交。
- **API 稳定**：CHANGELOG 显示 1.0→1.7 全是 `Added` / `Fixed`，未见破坏性重命名；1.0.0（2026-03-03）标志 API 定型。1.5.0 才加 `2025-11-25` 协议版本支持，说明仍在紧跟 MCP spec 演进，但旧 API 不会被推翻。
- **stdio 路径被官方持续打磨**：1.7.0 专门修了 stdio（PR #833 "reply -32700 on stdio parse errors instead of closing"），1.7.0 还新增 "task-based stdio examples"。stdio server 是一等公民。

### ⚠️ 对本项目的兼容性提醒

- rmcp 用 **edition 2024**，本项目 `src-tauri/Cargo.toml` 当前是 `edition = "2021"`。
  - edition 2024 需要 **Rust ≥ 1.85**（rmcp 自己 CI 用 1.92）。
  - **不需要**把整个 `tauri-app` 改成 edition 2024——Cargo 允许 workspace 内不同 crate 用不同 edition，rmcp 作为依赖只要本机 `rustc` 够新即可。需确认开发/CI 机 Rust 版本。
- rmcp `[lints.clippy]` 开了 `exhaustive_structs`/`exhaustive_enums` = warn，只影响 rmcp 自身，不影响下游。

---

## 2. 最小 stdio server 代码骨架

### 需要的 trait / 宏

| 名称 | 作用 |
|---|---|
| `#[tool_router]` | 标在 `impl Block` 上，收集该 impl 里所有 `#[tool]` 方法，生成 `Self::tool_router()` |
| `#[tool(description = "...")]` | 标在方法上，注册为一个 MCP 工具；方法名即工具名 |
| `#[tool_handler]` | 标在 `impl ServerHandler for X` 上，把 tool_router 接进 handler（1.4.0 后还能自动生成 `get_info`）|
| `ServerHandler` trait | server 的核心 trait，至少要能提供 `get_info()` |
| `rmcp::handler::server::wrapper::Parameters<T>` | 工具入参包装：`Parameters(MyArgs { .. }): Parameters<MyArgs>` 解构出参数 |
| `schemars::JsonSchema` (derive) | 入参结构体派生它 → 自动生成工具的 JSON Schema（`server` feature 已带 schemars）|
| `serde::Deserialize` (derive) | 入参结构体派生它 → 从 JSON 反序列化 |
| `rmcp::transport::stdio()` | 返回 stdio 传输（绑定进程的 stdin/stdout）|
| `rmcp::ServiceExt` | 提供 `.serve(transport)` 扩展方法 |
| `rmcp::model::*` | `CallToolResult`、`Content`、`ServerInfo`、`ServerCapabilities`、`Implementation` 等 |
| `rmcp::ErrorData as McpError` | 工具返回 `Result<CallToolResult, McpError>` 时的错误类型 |

### 入参 / 出参定义方式

- **入参**：定义一个 `struct`，派生 `serde::Deserialize + schemars::JsonSchema`；字段上用 `#[schemars(description = "...")]` 写字段说明。工具方法签名里用 `Parameters(MyArgs { a, b }): Parameters<MyArgs>` 取出。无参工具直接 `&self` 即可。
- **出参**：返回 `String`（最简，rmcp 自动包成 text content）或返回 `Result<CallToolResult, McpError>`（推荐，可控错误）。`CallToolResult::success(vec![Content::text(...)])` 构造成功结果。

### 是否强制 tokio

**是，强制依赖 tokio 异步运行时。** `rmcp` 的 `server` feature → `transport-async-rw` → `tokio/io-util`；`stdio()` 传输用 `tokio/io-std`；`.serve()` / `.waiting()` 都是 `async fn`。`main` 必须是 `#[tokio::main] async fn`。无法以纯同步方式使用。

### Cargo.toml（sidecar bin 部分）

本项目已是 cargo workspace（`src-tauri`），按对标 `miniterm-hook` 的方式新增一个 `[[bin]]`：

```toml
# src-tauri/Cargo.toml

[[bin]]
name = "miniterm-ssh-mcp"        # sidecar 名，自拟
path = "src/bin/miniterm-ssh-mcp.rs"

[dependencies]
# 已有: serde / serde_json ...
rmcp = { version = "1", features = ["server", "macros", "transport-io"] }
schemars = "1"                   # 入参 #[derive(JsonSchema)] 用；rmcp server 已传递依赖，显式列出更清晰
tokio = { version = "1", features = ["macros", "rt-multi-thread", "io-std"] }
tracing = "0.1"                  # 可选：日志门面
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }  # 可选：日志走 stderr
```

> feature 说明：`server`（工具系统 + 自动带 schemars）、`macros`（`#[tool]` 等宏）、`transport-io`（**服务端 stdio** 传输，注意不是 `transport-child-process`，那是客户端 spawn 子进程用的）。**不要**开 `transport-streamable-http-server` / `reqwest` / `auth`，否则会拖入 hyper/tower/reqwest 一大堆。

> tokio runtime：`rt-multi-thread` 也可换成 `rt`（单线程，更省）。stdio MCP server 负载极低，单线程 `rt` 足够；本骨架给 multi-thread 是保守选择。

### 最小可编译骨架

```rust
//! miniterm-ssh-mcp —— stdio MCP server sidecar
//! 暴露 ssh_list_connections / ssh_exec 给 Claude Code / Codex。

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SshExecArgs {
    #[schemars(description = "已保存 SSH 连接的 id 或 name")]
    connection: String,
    #[schemars(description = "在远程主机上执行的命令")]
    command: String,
}

#[derive(Clone)]
struct SshMcp;

#[tool_router]
impl SshMcp {
    /// 列出已保存的 SSH 连接（不含密码字段）
    #[tool(description = "List saved SSH connections (no passwords)")]
    async fn ssh_list_connections(&self) -> Result<CallToolResult, McpError> {
        // TODO: 读 config.json 的 sshConnections，剥掉 password
        let json = r#"[{"id":"1","name":"demo","host":"1.2.3.4","port":22,"user":"root"}]"#;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// 在指定 SSH 连接的远程主机上执行命令
    #[tool(description = "Execute a command on a remote host via a saved SSH connection")]
    async fn ssh_exec(
        &self,
        Parameters(SshExecArgs { connection, command }): Parameters<SshExecArgs>,
    ) -> Result<CallToolResult, McpError> {
        // TODO: portable-pty 跑 ssh + scan_ssh_prompt autofill；输出封顶 + 超时
        let out = format!("(stub) would run `{command}` on `{connection}`");
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[tool_handler]
impl ServerHandler for SshMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "SSH tools: list saved connections and run remote commands.".into(),
            ),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 铁律: 日志必须走 stderr, stdout 只留给 MCP 协议 JSON
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let service = SshMcp.serve(stdio()).await?;  // 握手 + 注册
    service.waiting().await?;                    // 阻塞直到 stdin 关闭 / 客户端断开
    Ok(())
}
```

> 注：官方 1.4.0+ 的 `#[tool_router]` / `#[tool_handler]` 可自动生成 `get_info` 与默认 router；上面显式写 `get_info` 是为了可控（设置 instructions、capabilities）。`anyhow` 仅 `main` 用，也可换成 `Box<dyn Error>` 省一个依赖。

### 官方可直接抄的示例

仓库 `examples/servers/src/` 下有大量 stdio server 示例（每个都是独立 `*_stdio.rs`）：

- `calculator_stdio.rs` + `common/calculator.rs` —— **最简骨架**，`#[tool_router(server_handler)]` 一个 impl 搞定，工具直接返回 `String`。本骨架即基于它。
- `counter_stdio.rs` + `common/counter.rs` —— 含带参工具、`Result<CallToolResult, McpError>` 返回、`ServerHandler` 手写实现、`get_info` 写法。
- `memory_stdio.rs` / `prompt_stdio.rs` / `structured_output.rs` / `task_stdio.rs` —— 进阶（带状态、prompt、结构化输出、长任务）。
- `examples/servers/src/` 全列表（main 分支）：`calculator_stdio.rs`、`counter_stdio.rs`、`completion_stdio.rs`、`memory_stdio.rs`、`prompt_stdio.rs`、`sampling_stdio.rs`、`elicitation_stdio.rs`、`task_stdio.rs`、`structured_output.rs` + 一批 streamable-http 示例。

---

## 3. 依赖体积评估

### rmcp 1.7.0 的依赖树

`rmcp` 1.7.0 共声明 **42 个依赖项**（含 build/dev）；其中 `normal` 依赖 32 个，但大部分是 **optional**（由 feature 决定是否启用）。

**用 `features = ["server", "macros", "transport-io"]` 时，实际会拉入的核心 crate（required + 被选中的 optional）：**

- 必带（无条件 required）：`tokio`、`tokio-util`、`futures`、`serde`、`serde_json`、`thiserror`、`tracing`、`async-trait`、`pin-project-lite`、`chrono`（默认特性已关，仅 `serde`+`now`）。
- `server`/`macros` 拉入：`schemars`（v1，JSON Schema 生成）、`pastey`、`rmcp-macros`、`base64`（default feature 带的）。
- **不会拉入**（因为没开对应 feature）：`reqwest`、`hyper`、`hyper-util`、`http`、`http-body*`、`bytes`、`tower-service`、`oauth2`、`jsonwebtoken`、`uuid`、`rand`、`url`、`sse-stream`、`process-wrap`、`which`。

`rmcp` crate 自身打包体积（crates.io `crate_size`）：**327 KB**（仅源码 .crate）。

### 真正的"重量"来自 tokio + futures + 它们的传递依赖

- `tokio`（v1，多线程 runtime）+ `tokio-util` + `futures` 是这个 sidecar 体积/编译时间的主要来源——会再传递拉入 `mio`、`socket2`、`libc`/`windows-sys`、`bytes`、`pin-project` 等。
- `schemars` v1 会拉入 `serde` 派生相关 + 它自己的运行时。
- 粗略量级（无法精确，未实跑 `cargo bloat`）：**这个 sidecar 的 release bin 预计 1.5 ~ 3 MB 量级**（Rust 静态链接、含 tokio）。编译期会比纯 `serde_json` bin 慢明显（tokio 是大 crate）。

### 与 `miniterm-hook` 对比

- `miniterm-hook`（`src-tauri/src/bin/miniterm-hook.rs`）刻意只用 `serde_json` + `std`，无 tokio、无 HTTP 客户端 crate，是个极致瘦身的"打了就跑就退出"的 bin。
- **本 sidecar 做不到那么瘦**：MCP 是长驻进程 + 异步协议握手，rmcp 强制 tokio。`miniterm-hook` 那种"纯 std"标准在这里不适用。
- 但相对一个 Tauri app（已经链接 webview、wry、几十个 crate）来说，多一个带 tokio 的 ~2-3 MB sidecar bin 是小事。tokio/serde 这些依赖项目主程序本就间接有（Tauri 依赖 tokio），workspace 共享编译产物，增量成本可控。

### 压缩体积的可选手段（若在意）

- tokio feature 用 `rt`（单线程）而非 `rt-multi-thread`，去掉 worker 线程池相关。
- release profile 加 `opt-level = "z"` / `lto = true` / `strip = true` / `panic = "abort"`（注意这些通常是 profile 级，会影响整个 workspace，需斟酌；可对 sidecar bin 单独评估）。
- 不开任何 HTTP/auth feature（本骨架已做到）。

---

## 4. 推荐方案 + 替代方案对比

### 推荐：直接用 `rmcp`

理由：
1. **官方 SDK**，跟 MCP spec 同步（已支持 `2025-11-25`），Claude Code / Codex 升级协议时跟得上。
2. 成熟（10M 下载、1.0 已发布、API 稳定）、活跃（每天提交、每月发版）。
3. stdio server 是一等公民，官方有多个 `*_stdio.rs` 示例可直接抄。
4. `#[tool]` 宏自动生成 JSON Schema，写工具几乎零样板。
5. 体积代价（tokio + ~15 个有效依赖、edition 2024）对一个 sidecar 完全可接受。

### 替代方案 A：其他 Rust MCP crate

crates.io 按下载排序的 MCP 生态（2026-05-18 实测）：

| crate | 最新版 | 累计下载 | 说明 |
|---|---|---|---|
| **rmcp** | 1.7.0 | **10,073,576** | 官方 SDK（本推荐）|
| `rust-mcp-sdk` | 0.9.0 | 129,969 | 第三方 SDK + 框架（server+client），仍 0.x |
| `rust-mcp-schema` | 0.10.0 | 386,809 | 只是类型定义（schema），不含 server 运行时 |
| `rust-mcp-transport` | 0.9.0 | 124,250 | `rust-mcp-sdk` 配套传输层 |
| `rust-mcp-macros` | 0.9.0 | 107,915 | `rust-mcp-sdk` 配套宏 |
| `rmcp-actix-web` | 0.12.9 | 147,008 | rmcp 的 actix-web 传输扩展（HTTP，本任务用不到）|
| `smg-mcp` | 2.2.0 | 493,322 | MCP **client** 实现，不是 server SDK |

**结论**：唯一严肃的替代是 `rust-mcp-sdk`（第三方），但下载量低 `rmcp` 约 80 倍、仍处 0.x（API 不稳定）。**没有理由弃官方 SDK 选它。** 其余要么只是 schema/类型、要么是 client、要么是 HTTP 扩展，都不替代 rmcp 在本任务的位置。

### 替代方案 B：手写 JSON-RPC over stdio

可行性：**技术上完全可行，但不推荐。**

- MCP over stdio = 逐行（newline-delimited）的 JSON-RPC 2.0 消息：客户端发 `initialize` → server 回 capabilities → `tools/list` → `tools/call` → ... 每条消息是一行 JSON。
- 手写要自己实现：JSON-RPC 2.0 信封（id/method/params/result/error）、`initialize` 握手 + 协议版本协商、`tools/list`（含每个工具的 JSON Schema，得手写 schema）、`tools/call` 分发、`notifications/*`、错误码（如 `-32700` 解析错误）、stdin 逐行读取循环。
- 优点：可做到像 `miniterm-hook` 那样**零第三方依赖（仅 `serde_json` + std）**，bin 极小、编译极快、无 tokio。stdio MCP server 是顺序请求-响应、无并发压力，纯同步 `stdin().lines()` 循环就能跑。
- 缺点：**工作量与维护成本明显**——MCP spec 在演进（rmcp 1.5 才加 2025-11-25 版本），手写要自己追协议；工具 schema 要手写 JSON（rmcp 用 `#[derive(JsonSchema)]` 自动生成）；边界情况（部分读、错误码、notification）容易踩坑。
- 工作量估计：一个**只读初版**（`initialize` + `tools/list` + `tools/call`，2 个工具，同步循环）大约 **200~400 行 Rust**，1~2 天能跑通；但要稳健处理协议演进/边界是持续负担。

**取舍**：除非"sidecar 必须零依赖、必须几百 KB"是硬约束，否则 `rmcp` 省下的协议工作量远大于它带来的体积代价。**推荐 rmcp**；只有当团队明确要把 sidecar 压到 `miniterm-hook` 那种极简度时才考虑手写。

---

## 5. stdio MCP server 已知坑

### 5.1 stdout 是协议专用通道（最重要）

- **stdout 只能输出 MCP 协议 JSON-RPC 消息**。任何 `println!` / 调试打印 / 库的意外输出混进 stdout，都会破坏 JSON-RPC 帧，导致客户端（Claude Code / Codex）解析失败、server 被判定挂掉。
- **所有日志必须走 stderr**。rmcp 官方示例统一这样配 `tracing`：
  ```rust
  tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).init();
  ```
  注意 `.with_ansi(false)` —— stderr 也别带 ANSI 颜色码，便于宿主收集日志。
- 任何被 sidecar 调用的子进程（这里是 `ssh`）的 stdout/stderr **绝不能继承到 sidecar 自己的 stdout**。`ssh_exec` 跑 ssh 时，子进程输出要捕获进 buffer 再作为 `CallToolResult` 返回，不能让它直通。本项目用 `portable-pty` 跑 ssh，PTY 输出本就是单独读取的，天然隔离——但要确保 PTY 数据不会被 `print` 到 stdout。

### 5.2 换行 / 缓冲

- JSON-RPC over stdio 是**逐行**的（每条消息一行、`\n` 结尾）。rmcp 内部处理好了帧分割；手写时务必每条消息 flush 且只追加单个 `\n`。
- **Windows 换行**：Windows stdout 文本模式可能把 `\n` 翻成 `\r\n`。Rust 标准 `io::stdout()` 默认是二进制式写入、不做 CRLF 转换，一般安全；但 rmcp 用 `tokio::io::stdout()`，同样不转换。**手写方案在 Windows 上要确认不被注入 `\r`**——多余的 `\r` 可能让对端 JSON 解析器报错。
- **缓冲/死锁**：stdio 是管道，写端不 flush，对端就收不到 → 握手卡死。rmcp 已处理 flush；手写时每写完一条消息必须 `flush()`。
- 读 stdin 要处理"部分读 / 一次读到多行 / 行被拆成两次 read"——按行缓冲累积，别假设一次 `read` 正好一条消息。

### 5.3 stdin 关闭 = 关闭信号

- 客户端关闭连接时会关 sidecar 的 stdin。rmcp 的 `service.waiting().await` 会在 stdin EOF / 客户端断开时返回——这是 sidecar 正常退出的信号，`main` 据此结束。
- sidecar 不要在 stdin 还开着时自己 `exit`；也要保证 stdin EOF 后能干净退出（别留挂起的 ssh 子进程——`ssh_exec` 的子进程要有超时和 kill 兜底，PRD 也提到了输出封顶 + 超时）。

### 5.4 协议解析错误不要直接崩

- rmcp 1.7.0 PR #833 改成"stdio 上遇到解析错误回 `-32700` 而不是关闭连接"。说明早期版本遇到坏 JSON 会断连。**用 ≥ 1.7.0 可获得这个健壮性修复**（再次支持"直接用最新版"的结论）。手写方案要自己实现：坏消息回 `-32700 Parse error`，不要 panic、不要退出。

### 5.5 启动期不要往 stdout 写任何东西

- sidecar 进程一旦启动，在 MCP `initialize` 握手完成前后，stdout 都属于协议。启动横幅、版本号、"server started" 之类**一律走 stderr**。
- 初始化失败（比如读不到 `config.json`）也别 panic 打到 stdout——记 stderr，必要时回结构化 JSON-RPC error。

### 5.6 与本项目相关的额外注意

- sidecar 没有 Tauri `AppHandle`，要像 `miniterm-hook.rs` 的 `get_port_file_path()` 那样，用 `dirs` 自己拼 `config.json` 路径（`{app_data_dir}/config.json`，app id `com.mini-term.app`）。
- 明文密码：`ssh_list_connections` 返回前必须剥掉 `password` 字段；错误信息也不能回显密码（PRD 验收项）。
- edition 2024 依赖要求开发机 / CI 的 `rustc` ≥ 1.85（rmcp CI 用 1.92，建议本地也升到接近版本以免编译报错）。

---

## 信息来源链接

- crate 主页 / 仓库：https://github.com/modelcontextprotocol/rust-sdk
- crate 文档：https://docs.rs/rmcp
- crates.io：https://crates.io/crates/rmcp
- crates.io API（版本/依赖/feature，本调研实际查询）：
  - `https://crates.io/api/v1/crates/rmcp`
  - `https://crates.io/api/v1/crates/rmcp/versions`
  - `https://crates.io/api/v1/crates/rmcp/1.7.0/dependencies`
- rmcp crate 的 `Cargo.toml`（feature 定义、依赖、MSRV 线索）：
  `https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/Cargo.toml`
- rmcp `CHANGELOG.md`（API 稳定性、stdio 修复 PR #833）：
  `https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/CHANGELOG.md`
- rmcp crate `README.md`（feature 表、transport 表）：
  `https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/README.md`
- 官方 stdio server 示例（骨架直接来源）：
  - `examples/servers/src/calculator_stdio.rs` + `examples/servers/src/common/calculator.rs`
  - `examples/servers/src/counter_stdio.rs` + `examples/servers/src/common/counter.rs`
- MCP 协议规范（rmcp 1.7.0 跟进的版本）：https://modelcontextprotocol.io/specification/2025-11-25

## Caveats / 不确定项

- 依赖树/bin 体积是基于 crate 声明 + 经验估算，**未实跑 `cargo build` / `cargo bloat`**。"1.5~3 MB"是量级估计，实际值需在本机编译后用 `cargo bloat --release` 确认。
- rmcp 没有在 Cargo.toml 写显式 `rust-version`（crates.io `rust_version` 字段为 `None`）；MSRV 通过 "edition 2024" + CHANGELOG "toolchain 1.92" 推断为 **≥ 1.85（edition 2024 下限）**，安全起见按 1.92 准备工具链。
- 骨架代码基于 main 分支示例（对应 1.7.x）整理，**未经编译验证**；`get_info` 里 `ServerInfo { .. }` 的字段集在不同小版本可能有 `#[non_exhaustive]` 约束，落地时以 `ServerInfo::new(...)`（见 `counter.rs` 示例第 224 行用法）这一 builder 形式更稳妥。
- 本调研用 `python-http-request` skill 查 crates.io / GitHub API + raw 源码完成；任务描述里提到的 `mcp__exa__*` 搜索工具在本次会话不可用，已用等价的官方一手数据源替代。

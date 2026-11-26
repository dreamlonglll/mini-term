# mini-term 全项目代码与架构评审报告

> **评审日期**：2026-11-26\
> **评审基线**：`main` @ `e0608f24`（`v1.14.7`）\
> **代码规模**：569 个源码文件 / 393,483 行（主工作区 18 crates + `sidecars/` + `relay-server/` + `mobile/`）\
> **用途**：记录本次全仓安全、稳定性、工程化与架构健康度评审结果，可作为后续整改任务书索引；完成后将状态改为 ✅ 并注明提交号。\
> **状态图例**：⬜ 待处理 · 🟡 进行中 · ✅ 已修复 · ⚪ 记档不修

---

## 一、代码库全景与工程度量

### 1.1 模块规模与测试密度（Rust，不含自动生成字典与图标表）

| 模块 | 文件数 | 生产代码行 | 测试代码行 | 测试占比 | `forbid(unsafe_code)` |
|------|-------:|-----------:|-----------:|---------:|:---------------------:|
| `crates/mt-app` | 186 | 119,248 | 47,221 | 28.4% | ❌（Win32/WebView） |
| `crates/mt-ui` | 38 | 17,397 | 8,961 | 34.0% | ✅ |
| `crates/mt-project` | 53 | 15,599 | 16,751 | 51.8% | ❌（git/encoding） |
| `crates/mt-ai` | 28 | 11,245 | 12,071 | 51.8% | ✅ |
| `crates/mt-app-core` | 39 | 10,174 | 10,460 | 50.7% | ✅ |
| `crates/mt-ssh` | 15 | 9,107 | 8,796 | 49.1% | ✅ |
| `crates/mt-remote` | 20 | 7,648 | 5,321 | 41.0% | ✅ |
| `sidecars` | 13 | 7,071 | 6,571 | 48.2% | ❌（Named Pipe/IPC） |
| `crates/mt-terminal` | 15 | 5,095 | 6,397 | 55.7% | ✅ |
| `crates/mt-relay` | 5 | 3,542 | 3,206 | 47.5% | ✅ |
| `crates/mt-usage` | 6 | 3,495 | 3,329 | 48.8% | ✅ |
| `crates/mt-config` | 4 | 2,865 | 3,028 | 51.4% | ✅ |
| `crates/mt-git-helper` | 9 | 2,293 | 3,002 | 56.7% | ✅ |
| `relay-server` | 11 | 2,245 | 4,065 | 64.4% | — |
| `crates/mt-pty` | 5 | 2,039 | 1,641 | 44.6% | ❌（ConPTY/ACL） |
| `crates/mt-core` | 10 | 1,011 | 1,379 | 57.7% | ✅ |
| `crates/mt-secret` | 2 | 659 | 356 | 35.1% | ❌（DPAPI） |
| `crates/mt-layout` | 1 | 437 | 263 | 37.6% | ✅ |
| `crates/mt-i18n` | 3 | 432 | 1,005 | 69.9% | ✅ |
| `crates/mt-theme-packs` | 1 | 389 | 372 | 48.9% | ✅ |
| **合计（Rust）** | — | **221,996** | **144,195** | **39.4%** | **14 / 18 crates** |

- 另含 `mobile/`（31 文件 / 5,062 行 TS/TSX）、`crates/mt-i18n/locales/*.ts`、`tests/` 架构守门测试等。

### 1.2 关键健康指标快照

- **生产 panic 风险极低**：在 22.2 万行 Rust 生产代码中，除去 `test_support.rs`（22 处）与 `build.rs`（3 处），全仓仅 **32 处** `.unwrap()` / `.expect()`，且大多数附带明确不变量说明。
- **`unsafe` 收敛集中**：全仓共 161 处 `unsafe` 出现点，集中在 Windows 平台 FFI（`clipboard.rs` 32、`ssh_key.rs` 28、`ipc.rs` 25、`tray.rs` 17、`native_view.rs` 6、`conpty.rs` 5、`dpapi.rs` 4 等），主工作区 14/18 个 crate 强制 `unsafe_code = "forbid"`。
- **架构守门测试**：`tests/layers.test.cjs`、`tests/relayProtocolParity.test.cjs`、`tests/rustCryptoFeatures.test.cjs`、`tests/gpuiMacFontKit.test.cjs` 均实测通过。
- **技术债标记极少**：全仓有效 `TODO`/`FIXME`/`HACK`/`XXX` 注释几乎为零。

---

## 二、核心工程亮点

1. **边界清晰且可机械验证的分层体系**：`AGENTS.md` 中的依赖铁律（如 `mt-core` 叶子限制、`mt-ui` 不碰 `mt-config`/`mt-i18n`/`mt-project`、单 `gpui-fast` 引擎、`sidecars` 不拉 `rusqlite`、中转 Rust/TS 协议字段一致性）全部编码进了 `tests/*.test.cjs` 并在 CI 中自动校验。
2. **纵深安全设计扎实**：
   - `mt-secret` 实现了 AES-256-GCM 凭据信封 + Windows DPAPI / Unix `0600` 主密钥封存 + SQLite `VACUUM` 迁移清洗；
   - `sidecars/src/ipc.rs` 实现了 Windows Named Pipe 双向身份核验（客户端同时校验服务端进程 Token SID 与管道对象 Owner SID）及 UAC 提权拒绝；
   - `mt-remote/src/git_deploy.rs` 实现了项目级远端 Git 授权绑定、SHA-256 校验、防符号链接及不覆写运行中 helper 的原子发布；
   - `crates/mt-app/src/file_viewer/sanitize.rs` 对远程 Markdown 与 AI 会话正文做了多轮不动点 GFM AST 清洗，不收敛时整体降级为缩进代码块；
   - `relay-server` 仅内存转发不落盘，支持多桌面隔离、单帧体积上限、有界出站队列与非 root 容器运行。
3. **注释即知识库**：代码注释对 ConPTY、OpenSSH、各家 AI CLI 实测行为及架构取舍有极高保真的记录。

---

## 三、问题汇总索引

> **二次定级与处理原则**：初评完成后结合实际威胁模型与维护成本做了二次分诊（见“复评定级 / 结论”列），本轮仅落地高性价比的 5 项（#1、#2、#3、#5 心跳、#9 Clippy 与 Audit），其余低风险/边角项留档或延后。

| # | 原定级 → 复评 | 维度 | 问题摘要 | 落点 | 复评结论 / 落地范围 | 状态 |
|---|---|---|---|---|---|---|
| 1 | P0 → **P1** | 安全 | 本地 Markdown 预览对任意 URI Scheme 无确认直调 `cx.open_url`（`ShellExecuteW` 风险） | `mt-app` (`file_viewer`) | 最该先修：仅放行 `mailto:`/`tel:` 且强制 `Confirm` 弹窗 | ✅ |
| 2 | P0 → **P2** | 安全 | HTML WebView 自定义协议 `read_file` 符号链接绕过扩展名白名单 | `mt-app` (`file_viewer`) | 仅修真实路径 MIME 复核返回 `403`；**不加**体积上限（避免副作用） | ✅ |
| 3 | P0 → **P1/P2** | 安全 | Hook HTTP 服务缺少浏览器跨站防护（CSRF / DNS Rebinding）且 `pty_id` 表无总数上限 | `mt-ai` (`hook_server`) | 向后兼容修法：`Host` 回环校验 + 拒 `Origin` / 跨站 `Sec-Fetch-Site` + `pty_id` 总量上限 512 | ✅ |
| 4 | P1 → **P3** | 安全/Bug | 主题包导入存在 `validate_theme_id` 漏拦 `.` 开头目录 Bug、Zip Bomb 风险与符号链接跟随 | `mt-theme-packs` | 边角问题，基本没有危害，暂缓 | ⬜ |
| 5 | P1 → **P2** | 稳定/安全 | 公网 `relay-server` 缺少服务端 WebSocket 心跳与空闲超时断开（原评其他项部分夸大） | `relay-server` | 仅落地服务端心跳（默认 30s `Ping` / 90s 空闲超时断开）；其余不做 | ✅ |
| 6 | P1 → **P3** | 安全/稳定 | 剪贴板临时目录在 Unix 多用户环境未隔离且权限过宽；Windows `read_bitmap` 巨额分配理论风险 | `mt-app` (`clipboard`) | 风险停留在理论上，暂缓 | ⬜ |
| 7 | P2 → **P3** | 安全/可靠 | 远程通知 `remote_notify` 响应体与重定向策略（部分属伪问题） | `mt-app` (`remote_notify`) | 暂缓 | ⬜ |
| 8 | P2 → **nit** | 安全卫生 | `mt-secret` 并发首启 TOCTOU / `OutBlob` 清零写法（部分属伪问题） | `mt-secret` | nit，暂缓 | ⬜ |
| 9 | **P2 维持** | 工程/CI | CI 补齐 `sidecars`/`relay-server` 变更行 Clippy 与三份 `Cargo.lock` 的 `cargo audit` | `.github` | 补齐 Clippy（含 sub-workspace 路径映射）与独立 `audit` job | ✅ |
| 10 | **P3** | 可维护性 | `mt-app` 多个头部 UI 文件过度膨胀（`project_list.rs` 达 6,231 行生产代码） | `mt-app` | 行数准确，此前评估过结论为缓拆 | ⬜ |
| 11 | P3 → **记档不修** | 构建/债务 | `Cargo.lock` 存在 108 个多版本重复 crate；存量 `rustfmt` / `clippy` 基线债 | 根工作区 | 数字准确，已在误报黑名单里，记档不修 | ⚪ |

---

## 四、问题详细分析与整改方案

### P0 — 高危安全缺陷

#### #1 本地 Markdown 预览对任意 URI Scheme 无确认直调 `cx.open_url`

- **相关代码**：
  - [crates/mt-app/src/file_viewer/markdown.rs](../crates/mt-app/src/file_viewer/markdown.rs#L601-L638)
  - [crates/mt-app/src/file_viewer/preview.rs](../crates/mt-app/src/file_viewer/preview.rs#L658-L670)
  - [crates/mt-app/src/file_viewer/webview.rs](../crates/mt-app/src/file_viewer/webview.rs#L368-L371) 与 [L640-L665](../crates/mt-app/src/file_viewer/webview.rs#L640-L665)
- **问题分析**：
  - `classify_link` 将 `http://` / `https://` 映射为 `LinkAction::External`（走 `Confirm` 弹窗），却把**所有其他满足 `[a-zA-Z][a-zA-Z0-9+.-]*:` 的协议**一律映射为 `LinkAction::Scheme(url)`。
  - `follow_link` 对 `LinkAction::Scheme(url)` 直接执行 `cx.open_url(&url)`（Windows 上对应 `ShellExecuteW`），**完全跳过用户确认弹窗**。打开不可信仓库中的本地 Markdown（如 `README.md`）并点击 `[文档](file:///C:/Windows/System32/calc.exe)`、`[资源](file://attacker/share/x.exe)`、`[搜索](search-ms:...)` 或 `ms-msdt:` 等链接，会直接触发系统协议处理器或 SMB 外连泄漏 NTLM 凭据。
  - 此外，HTML WebView 的 `with_new_window_req_handler` 会把页面 JS 零交互发起的 `window.open("mailto:...")` 转发到 `on_webview_link` → `follow_link` → 无确认拉起系统外部应用。
- **整改方案**：
  1. 在 `classify_link` 中对 `LinkAction::Scheme` 施加严格白名单（仅允许 `mailto:` 与 `tel:`，与 `sanitize.rs` 的 `remote_markdown_url_allowed` 保持一致），其余非 `http(s)` scheme 一律返回 `LinkAction::Ignore`。
  2. `follow_link` 对 `LinkAction::Scheme` 同样弹出 `Confirm` 确认框再调用 `cx.open_url`，消除 WebView 脚本无交互唤起外部程序的途径。
  3. 为 `file://`、`smb://`、`ms-msdt:`、`javascript:`、`vscode://` 补充单元测试断言拒绝。

---

#### #2 HTML WebView 自定义协议 `read_file` 符号链接绕过扩展名白名单

- **相关代码**：
  - [crates/mt-app/src/file_viewer/webview.rs](../crates/mt-app/src/file_viewer/webview.rs#L205-L245)（`plan_request`）
  - [crates/mt-app/src/file_viewer/webview.rs](../crates/mt-app/src/file_viewer/webview.rs#L465-L516)（`handle_request` / `read_file`）
- **问题分析**：
  - `plan_request` 校验的是请求 URL 的扩展名白名单（如放行 `.html` / `.js` / `.svg`，意图阻止读取 `.env` / `config.json` / 源码）。
  - 但当项目内存在符号链接（例如 `evil.html -> .env` 或 `leak.js -> secrets.json`）时：`plan_request` 看到 `evil.html` 放行 `Plan::File`；进入 `read_file` 后 `canonicalize(path)` 解析为 `<root>/.env`，因为仍在项目根内通过 `real.starts_with(root)`；第 495-498 行按真实文件名重算 `mime = mime_for(".env")` 得到 `None`，**但第 499-500 行没有在 `mime.is_none()` 时拒绝，直接 `std::fs::read(&real)` 并以 `200 OK` 返回了 `.env` 原文**。
  - 由于 WebView 允许脚本执行且无 CSP 限制外网请求，恶意 HTML 页面可借同源 `<iframe>`（或在 macOS WKWebView 缺省不附 `Sec-Fetch-Dest: empty` 时直接 `fetch`）读取项目内任意敏感文件并外带。
  - 同时，第 469 行（PDF）与第 499 行（HTML 静态资源）直接使用 `std::fs::read`，缺少 [file_viewer/http.rs](../crates/mt-app/src/file_viewer/http.rs#L76-L121) 的 32 MiB 硬上限与打开后普通文件检查，引用大文件可致 OOM。
- **整改方案**：
  1. 在 `read_file` 中严格校验真实路径的 MIME：`let Some(mime) = real.file_name().and_then(|n| n.to_str()).and_then(super::mime_for) else { return response(403, None, Vec::new()); };`。
  2. 为 `read_file` 与 PDF 读取引入普通文件检查及体积上限（如 HTML 静态资源 32 MiB，PDF 合理封顶如 128 MiB），使用 `File::open` + `metadata` + `take(limit + 1)` 防 TOCTOU。
  3. 补充符号链接指向 `.env` 返回 `403` 的回归单测。

---

#### #3 Hook HTTP 服务缺少浏览器跨站防护与 `pty_id` 总量上限

- **相关代码**：
  - [crates/mt-ai/src/hook_server.rs](../crates/mt-ai/src/hook_server.rs#L159-L218)（`HookTables`）
  - [crates/mt-ai/src/hook_server.rs](../crates/mt-ai/src/hook_server.rs#L1353-L1475)（`read_hook_request_body`）
- **问题分析**：
  - **浏览器 CSRF / DNS Rebinding**：`read_hook_request_body` 未校验 `Host` 头，也不拦截浏览器跨站请求头（`Origin`、`Sec-Fetch-Site`、`Sec-Fetch-Mode`），且不检查 `Content-Type`。由于 `fetch('http://127.0.0.1:23456/hook', { method: 'POST', mode: 'no-cors', headers: { 'Content-Type': 'text/plain' }, body: '...' })` 属于 CORS 简单请求（无 OPTIONS 预检），用户浏览器中打开的任意恶意网页（或本应用 HTML WebView 中打开的不可信 HTML）均可直接向本地 hook 端口注入伪造事件，触发误报提示音/远程通知并篡改指定 `pty_id` 的状态与 `SessionIdentity`（注：`agent::session_id_ok` 已有严格字符集白名单，阻止了进一步的命令注入）。
  - **`pty_id` 维度无界内存增长**：虽然单个 `pty_id` 内的子集合均已设限（`AGY_SUBAGENTS_CAP = 64`、`ACTIVE_SESSIONS_CAP = 8`、`ENDED_SESSIONS_CAP = 8`），但 `HookTables` 的 6 张 `HashMap<u32, ...>`（以及下游 `SessionTracker` 与 `StatusEmitter`）没有限制不同 `pty_id` 的总数。持续提交不存在的随机 `pty_id` 永远不会触发 `purge(pty_id)`，导致内存永久增长直至 OOM。
- **整改方案**：
  1. 在 `read_hook_request_body` 中增加完全向后兼容存量 CLI / Bun hook 的请求头守卫：
     - 校验 `Host` 头（若存在或 HTTP/1.1 必填）仅允许 `127.0.0.1` / `localhost`（可带端口）；
     - 拒绝任何携带 `Origin` 头，或携带 `Sec-Fetch-Site` 且值不为 `none` 的请求（返回 `403 Forbidden`）。
  2. 为 `HookTables` 增加总 `pty_id` 容量上限（如 1024）或注入活跃 PTY 判定回调，拒绝/淘汰未知 `pty_id` 的写入（同样保护 `SessionTracker` / `StatusEmitter` 不被虚假 `pty_id` 撑爆）。

---

### P1 — 中高风险（安全加固与功能 Bug）

#### #4 主题包导入：`.` 开头主题 ID Bug、Zip Bomb 与符号链接跟随

- **相关代码**：
  - [crates/mt-theme-packs/src/lib.rs](../crates/mt-theme-packs/src/lib.rs#L117-L119)（`list`）
  - [crates/mt-theme-packs/src/lib.rs](../crates/mt-theme-packs/src/lib.rs#L204-L258)（`import_zip`）
  - [crates/mt-theme-packs/src/lib.rs](../crates/mt-theme-packs/src/lib.rs#L291-L318)（`validate_theme_id` / `install_pack`）
- **问题分析**：
  1. **逻辑 Bug（与注释矛盾）**：`list()` 第 117 行过滤所有以 `.` 开头的目录名，并注明“真实主题 id 走 `validate_theme_id`，不会以 `.` 开头”；但 `validate_theme_id` 只拦了 `id == "."` 和 `id.contains("..")`，放行了 `.foo`、`.tmp-extract`。导入 `.hidden.zip` 后无法出现在主题列表中；导入 `.tmp-extract.zip` 会与暂存目录撞名并在第 256 行被删除。
  2. **Zip Bomb**：`import_zip` 直接调用 `archive.extract(&extract_dir)`，未限制文件总数与解压总字节数，构造的压缩炸弹可耗尽磁盘。
  3. **符号链接跟随**：`install_pack` 第 311 行 `path.is_file()` 与第 314 行 `fs::copy` 均跟随符号链接，可将外部敏感文件带入主题目录；`read_asset` 与 `verify_manifest` 同样无文件体积上限。
- **整改方案**：
  1. `validate_theme_id` 增加 `|| id.starts_with('.')`（及 Windows 尾随点/空格校验），并补充 `.hidden` / `.tmp-extract` 回归单测。
  2. `import_zip` 改为遍历 `archive.by_index(i)` 手工受控解压：限制最大条目数（如 64）、单文件与总解压字节上限（如 32 MiB）、使用 `enclosed_name()` 并显式跳过符号链接条目（`file.is_symlink()`）。
  3. `install_pack` 改用 `fs::symlink_metadata(&path)?.file_type().is_file()` 拒绝符号链接，并给 `read_asset` / `verify_manifest` 增加读取体积上限。

---

#### #5 公网 `relay-server` 鉴权限流、握手前大帧放大与 PWA 安全响应头

- **相关代码**：
  - [relay-server/server/src/lib.rs](../relay-server/server/src/lib.rs#L53-L84)
  - [relay-server/server/src/lib.rs](../relay-server/server/src/lib.rs#L513-L557)
  - [relay-server/server/src/lib.rs](../relay-server/server/src/lib.rs#L595-L645)
  - [mobile/src/relay/auth.ts](../mobile/src/relay/auth.ts#L24-L50)
- **问题分析**：
  1. **无鉴权失败速率限制**：`ConnPermit` 仅控制并发连接数，鉴权失败后立即释放名额，攻击者可循环快速暴力探测部署方设置的 `MT_RELAY_DESKTOP_KEY`；且 `secret_eq` 在长度不等时短路返回（内网/近距离场景可能暴露密钥长度），`KeyNotConfigured` 与 `InvalidKey` 区分也向未认证方暴露了服务端配置态。
  2. **未认证大帧内存放大 + 缺少服务端僵死探测**：`/ws/desktop` 在 `Hello` 握手之前即赋予 `16 MiB` 单帧上限，未认证方在 5s 握手窗内用单 IP 16 条连接即可撬动 256 MiB（全局 64 条达 1 GiB）内存峰值；另外已连接循环中仅依赖客户端 ping/TCP keepalive，缺少服务端主动 ping/读空闲超时，NAT 静默断连后旧槽位与 `ConnPermit` 会长期滞留。
  3. **PWA 静态服务缺少安全响应头**：`app_with_pwa` 未设置 `Content-Security-Policy`、`X-Frame-Options: DENY`、`X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`。移动端长期凭证存在 `localStorage` 且链接含 `#pair=`，应补齐标准 Web 安全头。
- **整改方案**：
  1. 增加按客户端 IP 的鉴权失败令牌桶/滑动窗口限流（例如每 IP 每分钟失败超过 10 次后返回 `429` 或强制冷却）；`secret_eq` 改为对双方先算 SHA-256（或 HMAC）再恒定时间比对。
  2. 为 `handle_desktop` / `handle_mobile` 消息循环增加服务端心跳（如 30s ping、90s 无帧超时断开）；评估缩小默认 `DESKTOP_MAX_MESSAGE_BYTES` 或将握手前的首帧限制与业务态解耦。
  3. 为 `app_with_pwa` 挂载 `tower_http::set_header` 安全响应头。

---

#### #6 剪贴板临时目录 Unix 多用户隔离缺失 & Windows `read_bitmap` 巨额分配风险

- **相关代码**：
  - [crates/mt-app/src/clipboard.rs](../crates/mt-app/src/clipboard.rs#L85-L129) 与 [L291-L297](../crates/mt-app/src/clipboard.rs#L291-L297)
  - [crates/mt-app/src/clipboard.rs](../crates/mt-app/src/clipboard.rs#L507-L534)
  - 对照标杆：[crates/mt-pty/src/ssh_key.rs](../crates/mt-pty/src/ssh_key.rs#L31-L38)
- **问题分析**：
  1. **Unix 共享 `/tmp` 暴露敏感粘贴内容**：`clipboard.rs` 直接写 `std::env::temp_dir().join("mini-term-clipboard")`，未按 `$XDG_RUNTIME_DIR` 或 `{euid}` 隔离目录、未设 `0700`/`0600` 权限、未拒绝目录/文件 symlink（`cleanup_old_files` 的 `entry.metadata()` 也会跟随 symlink），且仅按 `unix_millis()` 命名（同毫秒连续粘贴覆盖）。多用户 Linux 下他人可读用户粘贴到 AI 的长文本/截图或抢注符号链接。
  2. **Windows `read_bitmap` 巨额内存分配**：第 512 行仅限单边 `MAX_DIM = 65536` 而未限总像素数，面对 65536×65536 的 `HBITMAP`，第 533 行 `vec![0u8; buf_len]` 会直接请求 **16 GiB** 触发进程 OOM 中止；第 520 行 `CreateCompatibleDC(None)` 亦未判空句柄。
- **整改方案**：
  1. 参照 `mt-pty::ssh_key` 实现 Unix 下按用户隔离（`$XDG_RUNTIME_DIR` 优先，回退 `mini-term-clipboard-{euid}`）、`0700` 目录 + 属主校验 + `O_CREAT|O_EXCL` `0600` 文件 + 原子递增序号防碰撞，`cleanup_old_files` 改用 `symlink_metadata` 跳过符号链接。
  2. `win::read_bitmap`（及 `parse_dib`）增加总像素数上限（如 `MAX_PIXELS = 64_000_000`，即 256 MiB RGBA）并对 `CreateCompatibleDC` 返回值判空。

---

### P2 — 中等风险（可靠性、安全卫生与 CI 盲区）

#### #7 远程通知 `remote_notify`：无响应体上限、默认跟重定向、串行阻塞多渠道

- **相关代码**：
  - [crates/mt-app/src/remote_notify.rs](../crates/mt-app/src/remote_notify.rs#L376-L385) 与 [L474-L550](../crates/mt-app/src/remote_notify.rs#L474-L550)
  - [crates/mt-app/src/http.rs](../crates/mt-app/src/http.rs#L24-L31)
- **问题分析**：
  - `post()` 中 `resp.text().await` 无响应体大小上限（尽管实际仅用于解析简短 JSON 或截取 200 字符摘要），故障或恶意 Webhook 返回超大响应体将导致内存飙升。
  - `webhook_request` / `bark_request` / `telegram_request` 未校验 URL scheme 仅限 `http://`/`https://`；且共享 `reqwest::Client` 默认最多跟随 10 次重定向，携带敏感 URL Token 的推送遇到 302 时存在非预期转发风险。
  - `deliver()` 串行遍历 `Channel::ALL`，首渠道 10s 超时会阻塞后续渠道发送。
- **整改方案**：流式限读响应体（如 64 KiB）；校验仅允许 `http(s)://` 并对远程通知使用禁用重定向的客户端策略；`deliver()` 改用 `futures::future::join_all` 并行推送。

---

#### #8 `mt-secret` 并发建钥竞态与 DPAPI 缓冲区安全清零

- **相关代码**：
  - [crates/mt-secret/src/lib.rs](../crates/mt-secret/src/lib.rs#L153-L164) 与 [L321-L328](../crates/mt-secret/src/lib.rs#L321-L328)
  - [crates/mt-secret/src/dpapi.rs](../crates/mt-secret/src/dpapi.rs#L48-L58)
- **问题分析**：
  - `open_or_create` 先判断 `if path.exists()` 再调用 `write_key_file`（内部 `fs::rename(&tmp, path)` 在 Unix/Windows 都会覆盖目标文件），两个主程序实例并发首启时存在 TOCTOU 覆盖竞态。
  - `dpapi::OutBlob::drop` 使用 `self.as_slice().fill(0)` 清零明文密钥缓冲，未调用 crate 已引入的 `zeroize::Zeroize::zeroize`，存在被编译器死存储消除的理论风险；Unix 下 `encode_key_file` 返回的明文 base64 字符串也未使用 `Zeroizing<String>`。
- **整改方案**：`write_key_file` 改用 `fs::hard_link(&tmp, path)`（目标已存在时原子失败 `AlreadyExists` 再重读已有密钥，随后删 `tmp`）或 `OpenOptions::create_new(true)`；`OutBlob::drop` 换用 `zeroize::Zeroize::zeroize`；Unix `encode_key_file` 改用 `Zeroizing<String>`。

---

#### #9 CI 门禁存在 4 处覆盖盲区

- **相关代码**：
  - [.github/workflows/ci.yml](../.github/workflows/ci.yml#L183-L233) 与 [L287-L323](../.github/workflows/ci.yml#L287-L323)
  - [clippy.toml](../clippy.toml#L1-L2)
  - [relay-server/Dockerfile](../relay-server/Dockerfile#L7-L21)
- **问题分析**：
  1. **`sidecars/` 与 `relay-server/` 未跑 Clippy**：`clippy.toml` 注释声明覆盖这两个独立工作区，但 `ci.yml` 仅对根工作区跑了 `cargo clippy`。
  2. **`mt-app` 的 `cfg(windows)` 单测在 CI 中从未运行**：Linux CI 不编 Windows 分支，Windows CI 仅跑 `non_gpui_packages.mjs`（跳过了 `mt-app` / `mt-ui`），导致 `clipboard::win::tests`（4 条 DIB 越界安全测试）、`tray`、`frost`、`logfile` 等 Windows 专属单测处于 CI 盲区。
  3. **扩展验证脚本与供应链扫描未接入 CI**：`AGENTS.md` 中的 `bun run tools/omp-ext-check.ts`、`bun run tools/pi-ext-check.ts`，以及 `cargo audit` / `cargo deny` 尚未进 CI。
  4. **Dockerfile 未锁工具链/镜像版本**：`relay-server/Dockerfile` 使用浮动标签 `rust:1-bookworm` 与 `node:22-alpine`，与工作流严格钉死 `rust-toolchain.toml` 和 Action SHA 的工程标准不一致。
- **整改方案**：
  - 在 `ci.yml` 中为 `sidecars` 和 `relay-server` 补充变更行 `cargo clippy`；
  - 将 `clipboard::win::parse_dib` 等纯 Win32 解析逻辑下沉到 `mt-app-core` 或在 Windows CI 增加定向单测步；
  - 接入 `omp-ext-check.ts` / `pi-ext-check.ts`（或定期 workflow 跑 `cargo audit`），并在 `relay-server/Dockerfile` 中固定 Rust/Node 版本号或 digest。

---

### P3 — 架构债务与可维护性建议

#### #10 `mt-app` 头部 UI 文件过度膨胀（God File 热点）

`crates/mt-app/src/store/` 已拆分为 11 个聚焦文件（131～1,264 行），结构非常健康；但部分核心视图文件已膨胀至 2,500～6,200 行生产代码，增加理解成本、合并冲突与增量编译开销：

| 文件 | 生产代码行 | 测试行 | 最长函数 | 建议拆分边界 |
|------|-----------:|-------:|---------|-------------|
| `crates/mt-app/src/project_list.rs` | **6,231** | 734 | `render_rows` (399)、`render_group` (385)、`render_project` (351) | 拆为 `project_list/`：`icons.rs`、`reconcile.rs`、`menu.rs`、`dnd.rs`、`filter.rs`、`row.rs`、`group.rs` |
| `crates/mt-app/src/usage_panel/mod.rs` | **3,602** | 178 | `render_request_log_row` (278)、`render_overview_tab` (232) | 按 Tab 拆出 `overview.rs`、`chart.rs`、`top_sessions.rs`、`request_log.rs` |
| `crates/mt-app/src/pane.rs` | **3,376** | 328 | `new` (310)、`render` (118) | 拆出「PTY 基底」「内置 SSH 接管链路」「菜单与粘贴」 |
| `crates/mt-app/src/workbench_area.rs` | **2,779** | 388 | `render` (382) | 拆出 Tab 栏渲染、文档页签路由、关闭/重开历史 |
| `crates/mt-app/src/git/worktree.rs` | **2,611** | 686 | — | 分离弹窗状态机与表单渲染 |
| `crates/mt-app/src/ssh_panel.rs` | **2,585** | 282 | — | 分离连接/分组表单校验与面板渲染 |

---

#### #11 `Cargo.lock` 依赖多版本冗余与存量 Lint 债

- **多版本冗余**：`Cargo.lock` 当前存在 **108 个** 多版本并存 crate，包括 `windows-sys`（5 个版本）、`windows` / `windows-core`（4 个版本）、`phf`（5 个版本）、`rand` / `getrandom`（4 个版本）、`toml_edit`（4 个版本）。这会拉长冷编译时间并推高 CI `rust-cache` 占用（`ci.yml` 已逼近 10 GB 缓存额度）。建议在升级可控直接依赖时优先收拢到同一主版本。
- **存量 Clippy / Rustfmt 基线债**：目前 CI 通过 `check_changed_clippy.mjs` 与 `check_changed_rustfmt.mjs` 只拦变更行，属务实过渡方案；长期建议按小 crate 逐个清零存量 warning，逐步缩小忽略面。

---

## 五、分批落地建议

### 第一批：高危安全闭环（#1、#2、#3、#4）— 改动集中、性价比最高
- 收紧 `file_viewer` 本地 Markdown `LinkAction::Scheme` 白名单与确认弹窗（#1）；
- 修复 `webview.rs` 的 `read_file` 符号链接绕过扩展名白名单 + 补体积上限（#2）；
- 为 `hook_server` 增加 `Host` / `Origin` / `Sec-Fetch-Site` 校验与 `pty_id` 总容量上限（#3）；
- 修复 `mt-theme-packs` 的 `.` 开头主题 ID Bug、Zip Bomb 上限与符号链接防护（#4）。

### 第二批：网络与本机纵深加固（#5、#6、#7、#8）
- `relay-server` 补齐按 IP 鉴权失败限流、服务端 ping/pong 超时、恒定时间摘要比对及 PWA 安全响应头（#5）；
- `clipboard.rs` 对齐 `ssh_key.rs` 的 Unix 临时目录隔离规范，并给 Windows `read_bitmap` 补总像素上限与 `hdc` 判空（#6）；
- `remote_notify` 响应体流式限读、禁重定向、并行推送（#7）；
- `mt-secret` 消除 `open_or_create` 并发覆写窗口并改用 `zeroize()`（#8）。

### 第三批：工程化与架构重构（#9、#10、#11）
- 补齐 `ci.yml` 对 `sidecars`/`relay-server` Clippy、`mt-app` Windows 安全单测及 `omp`/`pi` 扩展检查的覆盖，固定 Dockerfile 镜像版本（#9）；
- 优先按子模块拆分 `project_list.rs` 与 `usage_panel/mod.rs`（#10）；
- 结合版本升级收拢 `Cargo.lock` 重复依赖并按 crate 渐进清零存量 Clippy warning（#11）。

---

## 六、本轮整改落地记录（2026-11-26）

依据二次分诊结论，已完成以下 5 项修复并通过对应测试与变更行 Lint 校验：

1. **#1 本地 Markdown / WebView 外部 Scheme 协议白名单与确认弹窗**
   - 修改 [crates/mt-app/src/file_viewer/markdown.rs](../crates/mt-app/src/file_viewer/markdown.rs)：`classify_link` 仅允许 `mailto:` 与 `tel:` 进入 `LinkAction::Scheme`，其余 scheme（`file://`、`smb://`、`search-ms:`、`ms-msdt:`、`javascript:`、`vscode://`、`data:` 等）统一归为 `LinkAction::Ignore`。
   - 修改 [crates/mt-app/src/file_viewer/preview.rs](../crates/mt-app/src/file_viewer/preview.rs)：`follow_link` 将 `LinkAction::Scheme` 与 `LinkAction::External` 合并走 `Confirm` 确认弹窗后再 `cx.open_url`。
   - 补充单测：[crates/mt-app/src/file_viewer/markdown_tests.rs](../crates/mt-app/src/file_viewer/markdown_tests.rs)。
2. **#2 HTML WebView `read_file` 真实路径 MIME 复核（不加体积上限）**
   - 修改 [crates/mt-app/src/file_viewer/webview.rs](../crates/mt-app/src/file_viewer/webview.rs)：抽出跨平台可测的 `resolve_served_file`，对 `canonicalize` 后的真实路径重算 `mime_for`，返回 `None` 时明确返回 `403 Forbidden`，阻断项目内符号链接（如 `evil.html -> .env`）绕过扩展名白名单；按复评意见不增加文件体积上限。
   - 补充单测：[crates/mt-app/src/file_viewer/webview_tests.rs](../crates/mt-app/src/file_viewer/webview_tests.rs)。
3. **#3 `hook_server` 浏览器跨站防护与 `pty_id` 总量封顶**
   - 修改 [crates/mt-ai/src/hook_server.rs](../crates/mt-ai/src/hook_server.rs)：增加 `is_loopback_host`（校验 `Host` 仅限 `127.0.0.1` / `localhost` / `[::1]` 含可选合法端口）、拒绝携带 `Origin` 或非 `none` 的 `Sec-Fetch-Site` 请求（返回 `403 Forbidden`，与 `miniterm-hook` 及 `omp`/`pi` 扩展完全兼容）；`HookTables` 新增 `MAX_TRACKED_PTYS = 512` 封顶防止伪造随机 `pty_id` 导致内存无限增长。
   - 补充 HTTP 跨站/DNS Rebinding 拦截与 `pty_id` 容量上限/回收恢复单测。
4. **#5 `relay-server` 服务端 WebSocket 心跳与空闲超时断开（仅做心跳）**
   - 修改 [relay-server/server/src/lib.rs](../relay-server/server/src/lib.rs)：新增默认 30s `Ping` 与 90s 空闲超时断开（`DEFAULT_PING_INTERVAL` / `DEFAULT_IDLE_TIMEOUT` / `RelayState::with_heartbeat`），覆盖 `handle_desktop` 与 `handle_mobile` 双向循环，静默断连及时释放 `ConnPermit` 与订阅槽位。
   - 新增集成测试：[relay-server/server/tests/heartbeat.rs](../relay-server/server/tests/heartbeat.rs)（3 例：保活存活、桌面静默断开、移动端静默断开）。
5. **#9 CI 补齐 `sidecars`/`relay-server` 变更行 Clippy 与 `cargo audit`**
   - 修改 [.github/scripts/check_changed_clippy.mjs](../.github/scripts/check_changed_clippy.mjs)：支持第 3 个可选参数 `[workspace-prefix]`，将子工作区编译器相对路径准确映射到仓库根 `git diff` 路径。
   - 修改 [.github/workflows/ci.yml](../.github/workflows/ci.yml)：在 Linux `rust` job 补齐 `sidecars` 与 `relay-server` 变更行 Clippy、在 `windows` job 补齐 `sidecars` 变更行 Clippy，并新增并行 `audit` job 对三份 `Cargo.lock` 运行 `cargo audit`。

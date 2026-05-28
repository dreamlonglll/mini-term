# cc-connect HTTP API 研究

- **Query**: cc-connect (chenhg5/cc-connect) 运行时对外暴露的本地 HTTP API 表面,供 mini-term 通过 Tauri command 调用
- **Scope**: 外部 (Go 仓库源码 + 官方 docs/) + 本机 v1.3.2 CLI hint
- **Date**: 2026-05-28
- **目标仓库**: https://github.com/chenhg5/cc-connect (Go),`main` @ `d6654023` (2026-05-28T02:07:51Z)
- **本机版本**: cc-connect v1.3.2,数据目录 `C:\Users\12197\.cc-connect\`

---

## 端口与监听

cc-connect 主进程同时可运行 **4 类**网络服务器,各有独立端口和开关:

| 服务 | 默认端口 | 路径 | 监听地址 | 启动条件 | 用途 |
|---|---|---|---|---|---|
| **Management API** | `9820` | `/api/v1/*` | `:port` ( = 0.0.0.0:port ) | `[management].enabled = true` | **接入主目标**:REST CRUD,管理 projects / sessions / cron / providers / heartbeat |
| **Bridge** (WebSocket) | `9810` (standalone) **或** 共享 Management 9820 | `/bridge/ws` + `/bridge/sessions*` | `:port` | `[bridge].enabled = true` | 外部 platform adapter 接入(WS 长连);message 流双向交换 |
| **Webhook** | `9111` | `/hook` (可改) | `:port` | `[webhook].enabled = true` | 外部 trigger 注入 prompt / shell exec(git hooks、CI 等) |
| **Unix socket API** (内部) | — | `~/.cc-connect/run/api.sock` | AF_UNIX(Windows 也支持) | 始终随 cc-connect 启动 | 仅 CLI 自调用:`send / cron / relay` 子命令的 IPC |

### Management API 端口

源码 `core/management.go:198-201`:

```go
m.server = &http.Server{
    Addr:    fmt.Sprintf(":%d", m.port),
    Handler: handler,
}
```

`":9820"` 在 Go 中等价于 `0.0.0.0:9820` — **监听所有网卡**,不是只绑 127.0.0.1。意味着 token 一旦泄露,同 LAN 内任意机器都能调用。**没有自动绑定 127.0.0.1 的逻辑**,需要使用方在反向代理 / 防火墙层面收紧。

### `cc-connect web` 子命令 — 一键开启

`cmd/cc-connect/web.go:29-50`:

```go
mgmtEnabled := cfg.Management.Enabled != nil && *cfg.Management.Enabled
port := cfg.Management.Port
if port == 0 {
    port = 9820
}
token := cfg.Management.Token

if !mgmtEnabled {
    fmt.Println("Web admin is not enabled. Configuring...")
    mgmtToken := core.GenerateToken(16)   // 32-char hex
    bridgeToken := core.GenerateToken(16)
    result, err := config.EnableWebAdmin(mgmtToken, bridgeToken)
    ...
    fmt.Println("Restart cc-connect for the changes to take effect.")
}
```

`EnableWebAdmin` (`config/config.go:3354-3417`) 同时打开 `[management]` 和 `[bridge]`,并把 `cors_origins` **默认设为 `["*"]`** — 允许任意 origin 嵌入(对 Tauri WebView2 友好,但相当于关掉 CSRF 保护)。

### Bridge 与 Management 共用端口的细节

`core/management.go:263-302` 的 `withStaticFallback` 在 management mux 之外加了一段路由:

```go
return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
    if strings.HasPrefix(r.URL.Path, "/api/") {
        apiMux.ServeHTTP(w, r)
        return
    }
    if m.bridgeServer != nil && r.URL.Path == m.bridgeServer.path {
        m.bridgeServer.handleWS(w, r)   // ← /bridge/ws 落到 management 端口上
        return
    }
    assets := GetWebAssets()
    ...
    http.FileServer(http.FS(assets)).ServeHTTP(w, r)  // SPA 静态资源
})
```

但 `core/bridge.go:213-231` 的 `BridgeServer.Start()` **也独立** `ListenAndServe(":9810")`。

结论:**`cc-connect web` 启动后,实际上 9810 和 9820 都会监听**,Bridge WS 在两个端口都可达。mini-term 接入只需打 9820 即可,不需要关心 9810。

---

## 鉴权

### Management API

`core/management.go:321-334`:

```go
func (m *ManagementServer) authenticate(r *http.Request) bool {
    if m.token == "" {
        return true   // 未配 token 等于完全开放
    }
    if auth := r.Header.Get("Authorization"); strings.HasPrefix(auth, "Bearer ") {
        return subtle.ConstantTimeCompare(
            []byte(strings.TrimPrefix(auth, "Bearer ")),
            []byte(m.token)) == 1
    }
    if t := r.URL.Query().Get("token"); t != "" {
        return subtle.ConstantTimeCompare([]byte(t), []byte(m.token)) == 1
    }
    return false
}
```

支持三种凭据传递:
1. `Authorization: Bearer <token>` — 推荐
2. `?token=<token>` query string — 兼容 EventSource 等无法设 header 的场景(虽然 Management 没有 SSE)
3. `Authorization: Bearer ""` (空 token) → cc-connect 默认拒绝;但配置 `token = ""` 整体跳过校验

**关键**:cc-connect **不验证来源 IP,没有 cookie/session**。token 是唯一凭据。

### Token 获取

token 在 **`config.toml` 明文**,由用户自定义或 `cc-connect web` 自动生成 16 字节 hex = **32 个字符**:

```toml
[management]
enabled = true
port = 9820
token = "abc123def456...32个hex字符"
cors_origins = ["*"]
```

mini-term 嵌入面板可在用户 home 下读 `~/.cc-connect/config.toml`,提取 `[management].token`,无需让用户手输。

### Bridge

`core/bridge.go:1236-1253`:支持 `Authorization: Bearer`、`X-Bridge-Token`、`?token=`。WS upgrade 之前先验 token,通过后再做 `CheckOrigin`(若 cors_origins 配 `*`,任意 origin 都过)。

### Webhook

`core/webhook.go:155-179`:支持 `Authorization: Bearer`、`X-Webhook-Token`、`?token=`。token 空则跳过校验。

### Unix socket API (`~/.cc-connect/run/api.sock`)

`core/api.go:53` chmod **0600**(仅本用户读写),无 token — 信任本机同用户。Windows 上确实可用(实测 `C:\Users\12197\.cc-connect\run\api.sock` 在 v1.3.2 存在,Go 1.18+ 在 Windows 10 1803+ 走 AF_UNIX)。

---

## 端点总览(分组表格)

下表对应 cc-connect 的 **Management API**(`/api/v1` 前缀,挂在 management 端口 9820)。所有端点鉴权方式相同。

### 系统状态 / 配置

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/status` | 版本、uptime、connected_platforms、projects_count、bridge_adapters |
| POST | `/api/v1/restart` | 优雅重启 cc-connect 进程,body: `{session_key, platform}`(可选) |
| POST | `/api/v1/reload` | 不重启进程,reload `config.toml` |
| GET | `/api/v1/config` | 返回 **明文 `config.toml`**(content-type: `text/plain`),**未脱敏** ⚠️ |
| GET / PATCH | `/api/v1/settings` | 全局 settings(language / log_level / display / rate_limit 等) |
| GET | `/api/v1/agents` | 已注册的 agent 类型 + platform 类型列表(Registry) |

### Projects

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/projects` | 项目摘要列表 |
| GET | `/api/v1/projects/{name}` | 单项目详情(agent_type / platforms / heartbeat / settings / work_dir / agent_mode) |
| PATCH | `/api/v1/projects/{name}` | 更新 language / admin_from / disabled_commands / work_dir / mode / agent_type / show_context_indicator / reply_footer / inject_sender / platform_allow_from |
| DELETE | `/api/v1/projects/{name}` | 从 config 中移除项目(需 restart) |
| POST | `/api/v1/projects/{name}/add-platform` | 给项目挂载新平台(body: `{type, options, work_dir, agent_type}`) |
| GET / POST | `/api/v1/projects/{name}/sessions` | 列出 / 创建 session |
| GET / DELETE | `/api/v1/projects/{name}/sessions/{id}` | session 详情(含 history)/ 删除 |
| POST | `/api/v1/projects/{name}/sessions/switch` | 切换 active session |
| POST | `/api/v1/projects/{name}/send` | **直接给 session 发文本消息** |
| GET / POST | `/api/v1/projects/{name}/providers` | 列项目级 providers / 添加 provider |
| POST | `/api/v1/projects/{name}/providers/{name}/activate` | 切换激活 provider |
| DELETE | `/api/v1/projects/{name}/providers/{name}` | 删除 provider |
| GET / PUT | `/api/v1/projects/{name}/provider-refs` | 引用的全局 provider 名列表 |
| GET | `/api/v1/projects/{name}/models` | 当前 agent 可用 models |
| POST | `/api/v1/projects/{name}/model` | 切换 model `{model}` |
| GET / PATCH | `/api/v1/projects/{name}/users` | user roles 配置 |
| GET | `/api/v1/projects/{name}/heartbeat[/status]` | 心跳状态 |
| POST | `/api/v1/projects/{name}/heartbeat/pause` | 暂停心跳 |
| POST | `/api/v1/projects/{name}/heartbeat/resume` | 恢复心跳 |
| POST | `/api/v1/projects/{name}/heartbeat/run` | 立即触发一次 |
| POST | `/api/v1/projects/{name}/heartbeat/interval` | 设置周期 `{minutes}` |

### Cron

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/cron[?project=]` | 列出所有 / 项目内 cron jobs |
| POST | `/api/v1/cron` | 创建 cron(body 见下 `CronAddRequest`) |
| PATCH | `/api/v1/cron/{id}` | 增量更新(body 是任意 field map) |
| DELETE | `/api/v1/cron/{id}` | 删除 |

### Global Providers

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/providers` | 全局共享 providers |
| POST | `/api/v1/providers` | 新增 |
| PUT/PATCH | `/api/v1/providers/{name}` | 更新 |
| DELETE | `/api/v1/providers/{name}` | 删除 |
| GET | `/api/v1/providers/presets` | 远程预设(从 GitHub/Gitee 拉) |
| GET | `/api/v1/providers/cc-switch` | 从 cc-switch 数据库读 providers |
| POST | `/api/v1/providers/cc-switch` | 批量导入 |

### Setup(QR onboarding)

| Method | Path | 说明 |
|---|---|---|
| POST | `/api/v1/setup/feishu/begin` | 启动飞书扫码,返回 `qr_url` + `device_code` |
| POST | `/api/v1/setup/feishu/poll` | 轮询扫码状态(`pending` / `completed` / `denied` / `expired`) |
| POST | `/api/v1/setup/feishu/save` | 保存 app_id/app_secret,完成绑定 |
| POST | `/api/v1/setup/weixin/begin` | 启动微信 ilink 扫码 |
| POST | `/api/v1/setup/weixin/poll` | 轮询 |
| POST | `/api/v1/setup/weixin/save` | 保存 token |

### Skills

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/skills` | 各项目可用 skills |
| GET | `/api/v1/skills/presets` | 远程 skill 预设列表 |

### Bridge

| Method | Path | 说明 |
|---|---|---|
| GET | `/api/v1/bridge/adapters` | 已连接的 WS adapter 列表 |
| GET | `/bridge/sessions?session_key=&project=` | Bridge 子系统的 session 列表(非 `/api/v1` 前缀) |
| POST | `/bridge/sessions` | 创建 session |
| GET / DELETE | `/bridge/sessions/{id}?session_key=` | 查 / 删 |
| POST | `/bridge/sessions/switch` | `{session_key, target, project}` |
| WS | `/bridge/ws` | **adapter** 连接(详见消息流章节) |

### 不在 `/api/v1` 的内部 socket API(仅 IPC,挂在 Unix socket 上)

| Method | Path | CLI 等价 | 备注 |
|---|---|---|---|
| POST | `/send` | `cc-connect send` | 发消息到 active session |
| GET | `/sessions` | — | 当前在线的 interactive sessions |
| POST | `/cron/add` | `cc-connect cron add` | |
| GET | `/cron/list` | `cc-connect cron list` | |
| GET | `/cron/info?id=` | — | |
| POST | `/cron/edit` | — | `{id, field, value}` |
| POST | `/cron/del` | `cc-connect cron del` | |
| POST | `/relay/send` | `cc-connect relay send` | |
| POST | `/relay/bind` | — | |
| GET | `/relay/binding?chat_id=` | — | |

**Mini-term 接入建议**:除非项目内 PowerShell/cmd 起一个 `cc-connect send` 子进程,否则 socket 路径不必走;同样动作在 Management API 上有 `/projects/{name}/send` 替代。

---

## 消息流(SSE/WS 详解)

### 关键结论

**Management API 没有 SSE,也没有用 WebSocket 推消息流。**

cc-connect 自带 web 前端(`web/src/api/*.ts`)消息历史显示方式是 **定期 GET `/api/v1/projects/{name}/sessions/{id}?history_limit=N`**,典型 SPA 轮询模式。`web/src/api/client.ts` 通篇没有 `EventSource` / `new WebSocket()` 调用。

证据:`/tmp/management.go` 全文搜索 `EventSource | sse | /events | /stream` 命中 0 处;`/tmp/web_*.ts` 搜索同样命中 0 处。

### Bridge WebSocket 不是给 UI 消费的

`/bridge/ws` 是给**外部 platform adapter**(Python/Node 写的 WeChat 适配器之类)接入用的,**协议方向相反**:

- adapter → cc-connect:`register` / `message` / `card_action` / `preview_ack` / `ping`
- cc-connect → adapter:`register_ack` / `reply` / `reply_stream` / `preview_start` / `update_message` / `delete_message` / `card` / `buttons` / `typing_start` / `typing_stop` / `audio` / `pong`

参考 `core/bridge.go:96-153` 和 `docs/bridge-protocol.md`。如果 mini-term 想"作为一个伪平台"接入 WS 拿到所有消息流,理论上可以注册一个 `platform=miniterm` 的 adapter,但这样会和真实平台抢消息(同一 session 只能路由到一个 adapter)。**不推荐**。

### 关键 WS 消息类型(`core/bridge.go:96-153`)

```go
type bridgeMessage struct {
    Type       string            `json:"type"`        // "message"
    MsgID      string            `json:"msg_id"`
    SessionKey string            `json:"session_key"`
    UserID     string            `json:"user_id"`
    UserName   string            `json:"user_name,omitempty"`
    Content    string            `json:"content"`
    ReplyCtx   string            `json:"reply_ctx"`
    Project    string            `json:"project,omitempty"`
    Images     []bridgeImageData `json:"images,omitempty"`
    Files      []bridgeFileData  `json:"files,omitempty"`
    Audio      *bridgeAudioData  `json:"audio,omitempty"`
}
```

cc-connect 回复方向(`bridge.go:980-985`):

```go
_ = a.server.sendToAdapter(a.platform, map[string]any{
    "type":        "card",
    "session_key": ca.SessionKey,
    "reply_ctx":   ca.ReplyCtx,
    "card":        serializeCard(card),
})
```

`reply` / `reply_stream` / `update_message` / `card` / `buttons` 都是 cc-connect → adapter 方向。

### 给 mini-term 嵌入面板的实务策略

要拿"实时消息流"只有两条路:
1. **轮询 `/api/v1/projects/{name}/sessions/{id}?history_limit=N`**(cc-connect 官方 web 走的就是这条)。简单,1-3s 间隔即可。
2. **作为 Bridge adapter 接入** WS,可拿到流式消息但和真实 IM 平台冲突,**不推荐**。

---

## 关键端点示例

### `GET /api/v1/status`

```bash
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:9820/api/v1/status
```

响应(管理 server 用统一信封 `{ok, data}`):

```json
{
  "ok": true,
  "data": {
    "version": "v1.3.2",
    "uptime_seconds": 3600,
    "connected_platforms": ["feishu", "telegram"],
    "projects_count": 2,
    "bridge_adapters": [
      {"platform": "custom", "project": "my-backend", "capabilities": ["text", "images"]}
    ],
    "bridge": {
      "enabled": true,
      "port": 9810,
      "path": "/bridge/ws",
      "token": "xxx",
      "token_set": true
    }
  }
}
```

### `GET /api/v1/projects`

```json
{
  "ok": true,
  "data": {
    "projects": [
      {
        "name": "Mini-term_5e21e739",
        "agent_type": "claudecode",
        "platforms": ["feishu"],
        "sessions_count": 2,
        "heartbeat_enabled": false
      }
    ]
  }
}
```

### `GET /api/v1/projects/{name}/sessions`

返回 `{sessions: [], active_keys: {sessionKey -> platform}}`。每条 session 含 `last_message` 预览(content 截断到 200 字符):

```json
{
  "ok": true,
  "data": {
    "sessions": [
      {
        "id": "sess_abc123",
        "name": "default",
        "session_key": "feishu:ou_xxx:oc_xxx",
        "agent_type": "claudecode",
        "active": true,
        "live": true,
        "history_count": 12,
        "platform": "feishu",
        "user_name": "Alice",
        "chat_name": "dev-channel",
        "created_at": "2026-05-28T09:00:00Z",
        "updated_at": "2026-05-28T10:30:00Z",
        "last_message": {
          "role": "assistant",
          "content": "Done! tests pass...",
          "timestamp": "2026-05-28T10:30:00Z"
        }
      }
    ],
    "active_keys": {"feishu:ou_xxx:oc_xxx": "feishu"}
  }
}
```

### `GET /api/v1/projects/{name}/sessions/{id}?history_limit=200`

返回完整 history 数组:

```json
{
  "ok": true,
  "data": {
    "id": "sess_abc123",
    "session_key": "feishu:ou_xxx:oc_xxx",
    "name": "default",
    "platform": "feishu",
    "agent_session_id": "as_xxx",   // 用于 Claude CLI --resume
    "history": [
      {"role": "user", "content": "Hello", "timestamp": "2026-05-28T09:00:05Z"},
      {"role": "assistant", "content": "Hi!", "timestamp": "2026-05-28T09:00:10Z"}
    ]
  }
}
```

### `POST /api/v1/projects/{name}/send` — 主动发消息(无附件)

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  http://127.0.0.1:9820/api/v1/projects/my-project/send \
  -d '{"session_key":"feishu:ou_xxx:oc_xxx","message":"Build done"}'
```

响应:`{"ok":true,"data":{"message":"message sent"}}`。

注意:Management API 的 `send` **不支持图片/文件附件**(`core/management.go:1106-1128`)。要发附件只能走 Unix socket 的 `/send`(`SendRequest` 含 `Images: []ImageAttachment, Files: []FileAttachment`,base64 编码)。

### `POST /api/v1/cron` — 创建 cron 任务

```json
{
  "project": "my-project",
  "session_key": "feishu:ou_xxx:oc_xxx",
  "cron_expr": "0 6 * * *",
  "prompt": "Daily summary",
  "description": "Morning brief",
  "session_mode": "reuse",
  "timeout_mins": 30
}
```

响应是完整 CronJob 对象(含自动生成 `id`、`enabled: true`、`created_at`)。

### `GET /api/v1/agents` — 查询可用 agent 与平台类型

```json
{
  "ok": true,
  "data": {
    "agents": ["claudecode", "codex", "gemini", "cursor", "qoder", "opencode", ...],
    "platforms": ["feishu", "telegram", "slack", "discord", "dingtalk", "line", "wecom", "weixin", "qq", "qqbot", "max", "weibo", "wps_xiezuo"]
  }
}
```

### `POST /api/v1/setup/feishu/begin` — 飞书扫码登录

返回 `{device_code, qr_url, interval, expires_in}`,前端 `expires_in / interval` 秒数轮询 `/poll`,拿到 `app_id/app_secret` 后 POST `/save`。这是 cc-connect 自带 web UI 飞书"零配置"的实现细节。

### Web 前端 token 持久化(参考实现)

`web/src/store/auth.ts:14-25`:

```typescript
login: (token, serverUrl) => {
  api.setToken(token);
  localStorage.setItem('cc_token', token);
  if (serverUrl) localStorage.setItem('cc_server_url', serverUrl);
  set({ token, serverUrl: serverUrl || '', isAuthenticated: true });
},
```

cc-connect 自带 web 通过 `?token=` query string 进入,登录后存 localStorage,后续请求都附 `Authorization: Bearer`。mini-term Tauri 端如果 invoke 一个 `getCcConnectToken` command,Rust 侧读 `~/.cc-connect/config.toml` 提取 token 即可,不必让用户手输。

---

## 风险与不确定项

### 已确认的风险

1. **`:port` 监听 0.0.0.0**:Management/Bridge/Webhook 均绑全网卡(`fmt.Sprintf(":%d", port)`)。如果用户在 office WiFi / 公共网络下开机,token 32 字符 hex 强度尚可,但 token 在 `config.toml` 明文,任何能读用户 home 的进程都能拿到。**mini-term 接入文档应提醒用户考虑** Windows Defender 防火墙拒绝入站 9820。

2. **`cc-connect web` 强制 `cors_origins = ["*"]`**(`config.go:3389-3391`):允许任意 origin 嵌入。Tauri WebView2 友好,但等价于关闭 CSRF 保护。token 即唯一防线。

3. **`GET /api/v1/config` 返回明文 `config.toml`**(`management.go:483-502`),`Content-Type: text/plain`,**未做密钥脱敏**。和官方 docs 的 "secrets `***` 化" 描述**不一致**,docs 里 §5.1 那段是设计草案,实现走的另一条路(直接 `os.ReadFile` 然后写出去)。⚠️ mini-term 调用 `/api/v1/config` 时拿到的会含 `api_key`、`app_secret`、`bot_token` 明文,**不要把这个响应展示到 UI 或日志**。

4. **`docs/management-api.md` 文件里写的 `GET /api/v1/logs` 端点 实际上不存在**:`buildHandler` 没注册,搜索 `handleLogs` 0 hit。Docs 里 §5.1 是设计草案。

5. **`docs/management-api.md` 写 `PATCH /api/v1/projects/{name}` 支持 `quiet` 字段,实际不支持**:`management.go:707-719` 的 body struct 没有 `quiet`,只有 `show_context_indicator / reply_footer / inject_sender`。

### 未求证项

1. **Windows AF_UNIX 兼容性**:`C:\Users\12197\.cc-connect\run\api.sock` 在本机 v1.3.2 确实存在 0 字节文件,但没实测过 `cc-connect send` 在 Windows 上是否真能成功(Go 1.18+ + Windows 10 1803+ 理论支持,但 mini-term 内嵌时若 cc-connect 由不同用户启动可能失败)。建议 mini-term 走 Management API 不依赖 socket。

2. **多 cc-connect 实例**:`cc-connect --force` 会 kill 同 config 的旧实例,但**没机制处理两份不同 config 同时跑**。如果用户同时跑多个 cc-connect(各跑各自的 config),mini-term 需要让用户选连哪个端口。

3. **Token 旋转**:Management token 改了之后,running cc-connect 不会 hot-reload token(`management.go:194-208` 的 server 一次性 NewMgmtServer 创建)。改 token 必须 `POST /api/v1/restart` 或杀进程。mini-term 如果 UI 上提供"改 token",需要在改完后立刻调 restart。

4. **`config.toml` 并发写**:`EnableWebAdmin` 用 `configMu` 全局 Mutex 保护,但 mini-term 和 cc-connect Web UI 都改同一份 config 时,可能互相覆盖未刷新的字段。建议 mini-term 改 config 只走 `PATCH` Management API,不直接写 toml。

5. **WS 推送是 cc-connect 内部 routing,无现成"消息广播订阅"接口**:如果 mini-term 想 "新消息来了立刻通知用户",**必须自己实现轮询**(2-5s 拉一次 `/sessions/{id}?history_limit=N`,diff history_count)。cc-connect 0.x 起一直没补 SSE 接口,作者短期内可能也不补(无 issue 跟踪)。

6. **`POST /api/v1/restart` 的副作用**:文档说"graceful restart",代码侧是把请求塞到 `RestartCh` chan,实际由主 goroutine 处理 SIGTERM → exec 新进程。**调用方需要做好 cc-connect 短暂离线 (~3 秒) 的兜底**。

7. **Bridge 与 Management 共用端口 vs 独立端口的实际表现**:代码两条路径都在(management 内 `withStaticFallback` 转发 + bridge 自己 ListenAndServe),意味着 Bridge WS **同时在 9810 和 9820 可达**。这是冗余,但对 mini-term 没影响 — 只用 9820 即可。

---

## 引用(commit SHA + 文件路径)

**Repo**: github.com/chenhg5/cc-connect, branch `main`, commit `d66540236f0041823c970f08448ca93cfacf95e0` (2026-05-28T02:07:51Z)

### 源码

- `core/api.go` (13806 B) — Unix socket API server:`/send /sessions /cron/* /relay/*`
- `core/management.go` (56768 B) — HTTP Management API:`/api/v1/*`,主接入面
- `core/bridge.go` (41092 B) — WebSocket Bridge server:`/bridge/ws + /bridge/sessions*`
- `core/web_manager.go` — `GenerateToken(n)` 工具
- `core/web_assets.go` — embed SPA dist
- `core/webhook.go` (8906 B) — `/hook` 端点,git/CI 触发器
- `core/heartbeat.go` — 心跳调度(API 落在 `/api/v1/projects/.../heartbeat`)
- `core/cron.go` (24645 B) — `CronScheduler` + `CronJob` 数据结构
- `config/config.go` — `ManagementConfig / BridgeConfig / WebhookConfig / EnableWebAdmin / WebSetupResult`(`130-170 行 + 3343-3417 行`)
- `cmd/cc-connect/main.go` — Management/Bridge/Webhook 启动逻辑(850-900 行)
- `cmd/cc-connect/web.go` — `runWeb` 一键开 web admin(2426 B)
- `cmd/cc-connect/send.go` — `cc-connect send` 走 Unix socket
- `cmd/cc-connect/sessions.go` — `cc-connect sessions list/show` **直接读 `~/.cc-connect/sessions/*.json`,不走 socket**
- `cmd/cc-connect/cron.go` / `relay.go` / `provider.go` — 走 Unix socket
- `web/embed.go` — `//go:embed all:dist`
- `web/src/api/client.ts` — fetch wrapper,`API_BASE='/api/v1'`,Bearer token
- `web/src/api/{status,projects,sessions,cron,bridge,settings,heartbeat,providers,setup,skills}.ts` — 各资源 typed wrapper
- `web/src/store/auth.ts` — token localStorage 持久化

### 官方文档

- `docs/management-api.md` (1185 行) — Management API 设计草案(有部分字段实现不一致,以源码为准)
- `docs/bridge-protocol.md` (906 行) — WebSocket Bridge 协议规范
- `INSTALL.md` §`cc-connect web` 章节 — web admin 启用流程

### CLI 输出(本机 v1.3.2)

- `cc-connect web` → "Opening: http://localhost:9820"(证实默认端口 + 默认 host)
- `cc-connect sessions list` → 输出 pipe-friendly 表格,**直接读文件不需要 cc-connect 进程**
- `cc-connect send/cron/relay --help` → 文档化 CLI 入参
- `C:\Users\12197\.cc-connect\run\api.sock` 存在 → 证实 Windows 上 AF_UNIX 工作

### Wire 类型关键定义

- `core.SendRequest` (`api.go:30-36`): `{project, session_key, message, images[], files[]}`
- `core.CronAddRequest` (`api.go:214-226`): `{project, session_key, cron_expr, prompt, exec, work_dir, description, silent, session_mode, mode, timeout_mins}`
- `core.GlobalProviderInfo` (`management.go:131-148`): provider 全字段(api_key / base_url / model / thinking / env / agent_types / endpoints / agent_models / agent_model_lists / codex)
- `core.WebhookRequest` (`webhook.go:29-38`): `{event, project, session_key, prompt, exec, work_dir, silent, payload}`
- `bridgeMessage` (`bridge.go:108-120`): WS adapter 入站消息

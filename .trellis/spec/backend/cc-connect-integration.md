# mini-term × cc-connect 集成约定

> mini-term 通过 cc-connect (chenhg5/cc-connect) Management API 实现:
> **进程启停管理** / **项目导入与关联** / **dashboard iframe 嵌入**。
> 下列契约必须严格遵守,否则会出现项目导入不生效、状态点误报红、
> iframe 被 Fluent 2 面板拽走等问题。

## Scope / Trigger

任何新增 cc-connect 集成功能(新 Tauri command / UI 入口 / 数据流)必须读完本 spec。

**参考实现**(均在 task `05-28-embed-cc-connect-panel` 落地):
- 后端:`src-tauri/src/cc_connect.rs`(8 个 Tauri command)
- 前端:`src/components/CcConnectStatusDot.tsx` + `CcConnectDashboard.tsx`
- hooks:`src/hooks/useCcConnectProbe.ts` + `useCcConnectProjects.ts`
- utils:`src/utils/ccConnectActions.ts`

## Signatures (HTTP 接入面)

cc-connect 主进程同时可跑 **4 类**网络服务器,但 **mini-term 只用 Management API 端口**:

| 服务 | 默认端口 | 路径 | mini-term 是否消费 |
|---|---|---|---|
| **Management API** | `9820` | `/api/v1/*` | ✅ 唯一接入点 |
| Bridge WebSocket | `9810` 或 9820 共享 | `/bridge/ws` | ❌ 给外部 adapter 用 |
| Webhook | `9111` | `/hook` | ❌ |
| Unix socket | `~/.cc-connect/run/api.sock` | — | ❌ 仅 cc-connect CLI 自调用 |

**端口 / token 位置**:`~/.cc-connect/config.toml`(Windows 上是 `%USERPROFILE%\.cc-connect\config.toml`):

```toml
[management]
enabled = true
port    = 9820
token   = "..."   # 32 字符 hex,cc-connect web 自动生成
cors_origins = ["*"]
```

## Contracts

### 鉴权

- 每个 `/api/v1/*` 请求**必须**带 `Authorization: Bearer <token>` 或 `?token=<token>` query
- `token = ""` 时 cc-connect 跳过校验(不安全) ── mini-term 端**必须强制非空**,空 token 返 diagnostic "执行 `cc-connect web` 自动生成"

### 关键端点

| 用途 | Method | Path | 说明 |
|---|---|---|---|
| 探活 + 版本 | GET | `/api/v1/status` | 5s timeout,失败返 `{ running: false, diagnostic }`,**不抛 Err** |
| 列项目 | GET | `/api/v1/projects` | 返 `{ ok, data: { projects: [{ name, work_dir, agent_type, platforms }] } }` |
| 改项目字段 | PATCH | `/api/v1/projects/{name}` | language / admin_from / work_dir / agent_type 等 |
| 删项目 | DELETE | `/api/v1/projects/{name}` | 删完**必须 restart**;`/reload` 不生效 |
| 重启 | POST | `/api/v1/restart` | execve 完整重启,**断所有 active sessions**,chat 历史保留 |
| reload | POST | `/api/v1/reload` | 仅遍历**已注册** engine 改配置,**对全新 [[projects]] 不生效** |

### 创建新项目的唯一路径

**cc-connect 没有 `POST /api/v1/projects` 端点。** 新增项目必经:

0. ⚠️ **每个 `[[projects]]` 必须带至少一个 `[[projects.platforms]]`**。cc-connect 的 `config.validate()`
   (config.go L1097-1103)对 `platforms` 为空的项目直接 `return error` → `main.go` `os.Exit(1)`
   (`config: projects[N] needs at least one [[projects.platforms]]`),**整进程冷启动失败**(经源码 + 隔离实测确认,
   v1.3.2)。没有 `enabled=false`/`disabled` 开关可「登记但不连」。导入时拿不到真实 IM 凭据 →
   **必须注入一个冷启动安全的占位平台**:`type="telegram"` + `options.token` 为**非空**假值
   (`make_project_table` 已硬编码 `PLACEHOLDER_PLATFORM_*`)。telegram 工厂仅校验 token 非空、`Start()`
   异步拨号失败只退避重连不崩;**绝不能用 discord**(其 `Start()` 同步 `session.Open()` 返回 error,单平台时拖垮 engine→os.Exit);
   **token 不可为空**(空串触发工厂 os.Exit)。用户后续在 Dashboard 把占位平台**替换**(而非删除)为真实平台。
   占位项目处于 pending 时 `/api/v1/status` 可能返 502 → 导入成功判定**以 `tomlWritten` 为准,不依赖 `status.running`**。
1. 用 `toml_edit` 往 `~/.cc-connect/config.toml` 追加 `[[projects]]`(含上面的占位平台,详见 [toml-edit-array-of-tables.md](./toml-edit-array-of-tables.md))
2. POST `/api/v1/restart`(**不能用 `/reload`**:Go 端 `handleReload` 当前 main 分支只遍历已注册的 engine,新项目永远不会被激活)
3. ⚠ restart 期间所有 **active IM sessions 被断开**,turn 中断;UI **必须显式 confirm** 提示用户"会重启 cc-connect,可能短暂中断 IM 连接,继续?"

### Dashboard iframe 嵌入

- cc-connect web 的 `Login.tsx` **支持** `?token=<token>` URL 参数自动登录
- cc-connect 不设 `X-Frame-Options` / CSP `frame-ancestors`(经 research 验证)
- URL 模板:`http://127.0.0.1:<port>/login?token=<encodeURIComponent(token)>&redirect=<encodeURIComponent(deep-link)>`
- deep-link 例:`/projects/<encodeURIComponent(name)>` 跳到对应项目页 ── 项目名**必须 URL 编码**,否则含 `/` `?` `#` 的项目名会破坏 redirect 参数解析
- ⚠ 嵌在 Fluent 2 `[data-panel]` 内必须 `createPortal` 到 `document.body`,见 [../frontend/fluent2-portal-modal.md](../frontend/fluent2-portal-modal.md)

## Validation & Error Matrix

| 条件 | 处理 |
|---|---|
| config.toml 缺失 | `probe` 返 `{ running: false, diagnostic: "无法定位配置目录" }` |
| [management] 段缺失 / token 为空 | 同上,diagnostic 提示跑 `cc-connect web` |
| HTTP 端口不通 | `probe` 返 `{ running: false, port, diagnostic: "GET ... 失败" }`,前端状态点变灰 |
| 项目名重复 | `cc_connect_import_project` 检 list + toml 双重防重;冲突加 8 字符 hash 后缀 |
| restart 失败但 toml 已写 | **不返 Err**:返 `ImportProjectResult { tomlWritten: true, restartOk: false, restartError: Some(...) }`。前端按 `tomlWritten` 仍写 `projectLinks` + 警告 toast,**避免"项目存在但未关联"半同步态**;**不回滚 toml**(用户可重试或手动重启 cc-connect 生效) |
| DELETE 成功但 restart 失败 | 同上语义:返 `UnlinkProjectResult { deletedOk: true, restartOk: false, restartError: Some(...) }`,前端按 `deletedOk` 仍清本地 `projectLinks` 摆脱 broken icon |
| 用户在 cc-connect web 手动删项目 | `useCcConnectProjects` 检测 `missingLinks`,ProjectList icon 标红 ⚠;右键加"清理失效关联"清本地 `projectLinks` |
| cc-connect 重启期间 list 暂时拉不到 | `useCcConnectProjects` **必须**仅在 `listLoaded` 后才比对,否则全员误报 broken |

## Good / Base / Bad Cases

- **Good**:探活成功 → 状态点绿 → 导入 confirm → toml_edit + restart → **立即手动 probe** 刷新 → icon 变绿 → 一键打开 dashboard 配 platform
- **Base**:cc-connect 没启动 → 状态点灰 → 设置里点"启动" → spawn 成功 → 5s 内变绿(全局轮询)
- **Bad**:导入 `[[projects]]` 不带任何 `[[projects.platforms]]` → cc-connect 下次冷启动 `validate()` 失败 `os.Exit(1)`(曾导致整个导入功能被删,见 commit 6cb688d);用 `POST /api/v1/reload` 期望新项目生效 → 静默无效;用 BurntSushi/toml 序列化 config.toml → 用户注释全丢;`probe` 抛 Err → 前端轮询断了无法降级

## Tests Required

1. **probe 不抛错的契约**:`cc_connect_probe` 在配置缺失 / 端口不通 / token 为空场景下必须返 `CcConnectStatus { running: false, diagnostic: Some(...) }`,**不返 `Result::Err`**(轮询前端无法降级)
2. **toml round-trip**:导入流程的核心 toml_edit 行为(参考 [toml-edit-array-of-tables.md](./toml-edit-array-of-tables.md) Tests Required)
3. **URL 编码**:项目名含 `/` 时 `redirect` deep-link 必须正确编码,前端 `openCcDashboardForProject` 用 `encodeURIComponent`
4. **race-safe broken 标记**:`useCcConnectProjects` 在 cc-connect 重启期间 list 暂时拉不到时,**不能**把已关联项目标 broken;`listLoaded` 状态控制比对时机
5. **重启 confirm 强制**:import / unlink 入口必须经 `showConfirm`(或等价),不允许静默 restart

## Wrong vs Correct

### Wrong

```rust
// ❌ 用 reload 期望新项目生效
http_post_json(&url(port, "/api/v1/reload"), &token, &json!({}))?;
// 静默成功但 Go handleReload 不创建新 engine,[[projects]] 永远不激活
```

```typescript
// ❌ iframe URL 直接拼项目名
const url = `http://127.0.0.1:${port}/login?token=${token}&redirect=/projects/${name}`;
// name 含 / ? # 时 redirect 参数解析错位
```

```typescript
// ❌ useEffect 监听 configPath 反复 probe
const probe = useCallback(async () => { ... }, [configPath]);
useEffect(() => { probe(); }, [probe]);
// 用户输入 configPath 时每按一次键 probe 一次,半成品路径触发状态点闪烁
```

```typescript
// ❌ list 失败时清空 ccProjects,导致 missingLinks 含所有已关联项目
const refresh = async () => {
  try { setCcProjects(await invoke('cc_connect_list_projects')); }
  catch { setCcProjects([]); }   // race: cc-connect 重启 5s 内全员标 broken
};
```

### Correct

```rust
// ✓ 创建项目走 toml_edit + restart
write_toml_with_new_project(&path, &name, &work_dir)?;
http_post_json(&url(port, "/api/v1/restart"), &token, &json!({}))?;
```

```typescript
// ✓ token 与 name 都 encodeURIComponent
const deepLink = `/projects/${encodeURIComponent(name)}`;
const url = `http://127.0.0.1:${port}/login?token=${encodeURIComponent(token)}&redirect=${encodeURIComponent(deepLink)}`;
```

```typescript
// ✓ probe 只 mount 探一次,后续靠 5s 全局轮询保鲜
const configPathRef = useRef(configPath);
configPathRef.current = configPath;
useEffect(() => { probe(); /* 用 configPathRef.current 读最新值 */ }, []);
```

```typescript
// ✓ race-safe:仅 listLoaded 后才计算 broken,失败保留上一轮
const [listLoaded, setListLoaded] = useState(false);
const refresh = async () => {
  try {
    const list = await invoke<CcProject[]>('cc_connect_list_projects', { configPath });
    setCcProjects(list);
    setListLoaded(true);
  } catch { /* keep previous list */ }
};
const missingLinks = listLoaded
  ? Object.entries(projectLinks).filter(([_, name]) => !ccProjects.some(p => p.name === name))
  : [];
```

## Related

- [toml-edit-array-of-tables.md](./toml-edit-array-of-tables.md) ── TOML 写回机制
- [tauri-command-nested-args.md](./tauri-command-nested-args.md) ── 8 个 cc-connect command 含 struct 参数的 invoke 约定
- [../frontend/fluent2-portal-modal.md](../frontend/fluent2-portal-modal.md) ── iframe 在 Fluent 2 下的 portal 约定
- [../guides/cross-layer-thinking-guide.md](../guides/cross-layer-thinking-guide.md) ── 跨层契约思考清单
- Task:`.trellis/tasks/05-28-embed-cc-connect-panel/`(完整 PRD + 5 份 research)
- 上游:<https://github.com/chenhg5/cc-connect>

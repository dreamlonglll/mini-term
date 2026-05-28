# Research: cc-connect 创建新项目的实际路径

- **Query**: cc-connect 的"创建新项目"具体走什么端点? 完整 management API 路由清单 + web 前端 add-project 调用代码 + reload 行为对 active sessions 的影响
- **Scope**: external (上游仓库 chenhg5/cc-connect)
- **Date**: 2026-05-28
- **Upstream commit**: main @ `d66540236f0041823c970f08448ca93cfacf95e0`

---

## 1. 完整 management API route 清单(全 HandleFunc)

源码：`core/management.go`, function `buildHandler`, line 210-251。

```go
// 系统级
mux.HandleFunc(prefix+"/status",   m.wrap(m.handleStatus))
mux.HandleFunc(prefix+"/restart",  m.wrap(m.handleRestart))
mux.HandleFunc(prefix+"/reload",   m.wrap(m.handleReload))
mux.HandleFunc(prefix+"/config",   m.wrap(m.handleConfig))       // GET only(读 raw toml)
mux.HandleFunc(prefix+"/settings", m.wrap(m.handleGlobalSettings)) // GET / PATCH 全局配置

// Agents 注册表
mux.HandleFunc(prefix+"/agents", m.wrap(m.handleAgents))

// Projects
mux.HandleFunc(prefix+"/projects",  m.wrap(m.handleProjects))         // GET only -> 列表
mux.HandleFunc(prefix+"/projects/", m.wrap(m.handleProjectRoutes))    // GET/PATCH/DELETE + 各子路由

// Cron
mux.HandleFunc(prefix+"/cron",  m.wrap(m.handleCron))
mux.HandleFunc(prefix+"/cron/", m.wrap(m.handleCronByID))

// QR 引导式 setup(只支持 feishu/weixin)
mux.HandleFunc(prefix+"/setup/feishu/begin", m.wrap(m.handleSetupFeishuBegin))
mux.HandleFunc(prefix+"/setup/feishu/poll",  m.wrap(m.handleSetupFeishuPoll))
mux.HandleFunc(prefix+"/setup/feishu/save",  m.wrap(m.handleSetupFeishuSave))
mux.HandleFunc(prefix+"/setup/weixin/begin", m.wrap(m.handleSetupWeixinBegin))
mux.HandleFunc(prefix+"/setup/weixin/poll",  m.wrap(m.handleSetupWeixinPoll))
mux.HandleFunc(prefix+"/setup/weixin/save",  m.wrap(m.handleSetupWeixinSave))

// 全局 Providers
mux.HandleFunc(prefix+"/providers",  m.wrap(m.handleGlobalProviders))
mux.HandleFunc(prefix+"/providers/", m.wrap(m.handleGlobalProviderRoutes))

// Skills
mux.HandleFunc(prefix+"/skills",         m.wrap(m.handleSkills))
mux.HandleFunc(prefix+"/skills/presets", m.wrap(m.handleSkillPresets))

// Bridge
mux.HandleFunc(prefix+"/bridge/adapters", m.wrap(m.handleBridgeAdapters))

// SPA static fallback
return m.withStaticFallback(mux)
```

`handleProjectRoutes` 的内部子路由分发(line 575-633)，对路径 `/api/v1/projects/{name}/{sub}/{rest}` 做 switch：

| sub | 处理函数 | 备注 |
|---|---|---|
| `""` (空) | `handleProjectDetail` | **GET / PATCH / DELETE** |
| `add-platform` | `handleProjectAddPlatform` | **POST**，**实际的创建项目入口** |
| `sessions` | `handleProjectSessions` | |
| `send` | `handleProjectSend` | |
| `providers` | `handleProjectProviders` | |
| `provider-refs` | `handleProjectProviderRefs` | |
| `models` | `handleProjectModels` | |
| `model` | `handleProjectModel` | |
| `heartbeat` | `handleProjectHeartbeat` | |
| `users` | `handleProjectUsers` | |

**确认：management API 路由表里没有任何 `POST /api/v1/projects`、`POST /api/v1/setup/projects`、`PUT /api/v1/config`、`POST /api/v1/onboard` 之类的"显式创建项目"端点**。

---

## 2. 创建项目的实际路径(answered)

候选答案对照原问题：

- **(a) ✅ 漏掉的端点**：`POST /api/v1/projects/{name}/add-platform`
- **(b) ✅ setup 子路径(QR 引导)**：`POST /api/v1/setup/feishu/save` 和 `POST /api/v1/setup/weixin/save`(只对飞书/微信生效)
- (c) ❌ 不存在 `PUT /api/v1/config`：`handleConfig` (management.go 483-502) **只允许 GET**，返回 raw config.toml 文本
- (d) ⚠️ 部分正确：直接写 toml + `POST /reload` **当前 main 分支不能完整生效**(详见第 4 节)

### 2.1 路径 (a) — POST /api/v1/projects/{name}/add-platform

源码：`core/setup.go` line 458-491。

```go
type AddPlatformRequest struct {
    Type      string         `json:"type"`        // 必填(eg "feishu", "telegram", "discord")
    Options   map[string]any `json:"options"`     // platform 字段(eg telegram bot_token)
    WorkDir   string         `json:"work_dir"`    // 新项目时使用
    AgentType string         `json:"agent_type"`  // 新项目时使用(claudecode/codex/gemini/...)
}

func (m *ManagementServer) handleProjectAddPlatform(w http.ResponseWriter, r *http.Request, projectName string) {
    if r.Method != http.MethodPost { ... }
    var req AddPlatformRequest
    json.NewDecoder(r.Body).Decode(&req)
    if req.Type == "" { mgmtError(w, 400, "type is required") }
    // 调用注入回调,实际由 main.go 中的闭包路由到 config.AddPlatformToProject
    m.addPlatformToProject(projectName, req.Type, req.Options, req.WorkDir, req.AgentType)
    mgmtJSON(w, 201 /* Created */, map[string]any{
        "message":          fmt.Sprintf("platform %q added to project %q", req.Type, projectName),
        "restart_required": true,
    })
}
```

**关键 upsert 语义** — `config/config.go` line 3080-3124 `AddPlatformToProject`：

```go
func AddPlatformToProject(projectName string, platform PlatformConfig, workDir, agentType string) error {
    // ... 读 cfg ...
    for i := range cfg.Projects {
        if cfg.Projects[i].Name == projectName {
            cfg.Projects[i].Platforms = append(cfg.Projects[i].Platforms, platform)
            return saveConfig(cfg)     // 已存在项目 → append platform
        }
    }
    // 不存在 → 新建一个 ProjectConfig 并 append
    agentCfg := AgentConfig{Type: "codex", Options: map[string]any{}}
    if at := strings.TrimSpace(agentType); at != "" {
        agentCfg.Type = at
    }
    if len(cfg.Projects) > 0 && at == "" {
        agentCfg = cloneAgentConfig(cfg.Projects[0].Agent)  // 复用首个项目的 agent
    }
    if wd := strings.TrimSpace(workDir); wd != "" {
        agentCfg.Options["work_dir"] = wd
    }
    cfg.Projects = append(cfg.Projects, ProjectConfig{
        Name:      projectName,
        Agent:     agentCfg,
        Platforms: []PlatformConfig{platform},
    })
    return saveConfig(cfg)  // 写回 config.toml
}
```

**这是 cc-connect 创建新项目的事实标准入口**：路径名叫 `add-platform`，但行为是 upsert(project 不存在则创建)。当前 main 分支 docs/management-api.md **未列入此端点**，是未文档化的 internal API。

### 2.2 路径 (b) — POST /api/v1/setup/feishu/save & /setup/weixin/save

源码：`core/setup.go` line 190-216 (feishu)，line 428-454 (weixin)。

请求 body：
```typescript
// /api/v1/setup/feishu/save
{
  project: string;       // 必填,项目名
  app_id: string;        // 必填,飞书 App ID
  app_secret: string;    // 必填,飞书 App Secret
  platform_type: string; // "feishu" 或 "lark"
  owner_open_id?: string;
  work_dir?: string;
  agent_type?: string;
}

// /api/v1/setup/weixin/save
{
  project: string;
  token: string;          // 必填
  base_url?: string;
  ilink_bot_id?: string;
  ilink_user_id?: string;
  work_dir?: string;
  agent_type?: string;
}
```

底层实现(`cmd/cc-connect/main.go` line 902-925)调用 `config.EnsureProjectWithFeishuPlatform` / `config.EnsureProjectWithWeixinPlatform`，本质和 `AddPlatformToProject` 同样：项目不存在则插入新 `[[projects]]`。**只能用于 feishu 或 weixin，不适用 telegram/discord/slack/...**

返回：`{ message, restart_required: true }`

### 2.3 路径 (c) 不存在 — PUT /api/v1/config 没有实现

`handleConfig` (`core/management.go` line 483-502) **only GET**：
```go
func (m *ManagementServer) handleConfig(w http.ResponseWriter, r *http.Request) {
    if r.Method != http.MethodGet {
        mgmtError(w, http.StatusMethodNotAllowed, "GET only")
        return
    }
    data, _ := os.ReadFile(m.configFilePath)
    w.Header().Set("Content-Type", "text/plain; charset=utf-8")
    w.Write(data)  // 整个 config.toml 文本(密钥未脱敏！注意权限)
}
```

---

## 3. web 前端 add-project 调用代码片段

源码：`web/src/pages/Projects/ProjectList.tsx` 1-265 行 + `PlatformManualForm.tsx` 1-101 行。

### 3.1 ProjectList.tsx — wizard 顶层

```typescript
// 步骤: name → platform → qr | form | done
const [wizStep, setWizStep] = useState<'name' | 'platform' | 'qr' | 'form' | 'done'>('name');

// 步骤 1: 填项目名 / 工作目录 / Agent 类型
const AGENT_OPTIONS = [
  { key: 'claudecode', label: 'Claude Code' },
  { key: 'codex',      label: 'Codex' },
  // gemini / cursor / devin / acp / opencode / qoder / ...
];

// 步骤 2: 选 platform(feishu / weixin / telegram / discord / slack / ...)
const isQRPlatform = (type: string) => type === 'feishu' || type === 'lark' || type === 'weixin';

const handlePlatformSelect = (key: string) => {
  if (isQRPlatform(key)) {
    setWizStep('qr');     // 飞书 / 微信走扫码
  } else if (platformMeta[key]) {
    setWizStep('form');   // 其他平台走手填表单
  } else {
    setWizStep('done');
  }
};
```

### 3.2 PlatformManualForm.tsx — 非 QR 平台的"添加"实际调用

```typescript
import { addPlatformToProject } from '@/api/projects';

const handleSave = async () => {
  const opts: Record<string, any> = {};
  for (const f of meta.fields) {
    const v = values[f.key];
    if (v !== undefined && v !== '' && v !== false) opts[f.key] = v;
  }
  await addPlatformToProject(projectName, {
    type: platformType,
    options: opts,
    work_dir: workDir,
    agent_type: agentType,
  });
  onComplete();
};
```

### 3.3 web/src/api/projects.ts — 调用封装

```typescript
export const addPlatformToProject = (projectName: string, body: {
  type: string; options: Record<string, any>; work_dir?: string; agent_type?: string;
}) => api.post<{ message: string; restart_required: boolean }>(`/projects/${projectName}/add-platform`, body);

// 注意:projects.ts 文件里没有 createProject / newProject!只有 list/get/update/addPlatform/delete
export const listProjects   = () => api.get<{ projects: ProjectSummary[] }>('/projects');
export const getProject     = (name) => api.get<ProjectDetail>(`/projects/${name}`);
export const updateProject  = (name, body) => api.patch(`/projects/${name}`, body);
export const deleteProject  = (name) => api.delete<{ message; restart_required }>(`/projects/${name}`);
```

### 3.4 add-platform 完成后是否自动调用 restart?

**关键观察 — PlatformManualForm.tsx 没有自动调用 restart**。grep 该文件，无 `restart` 字样。完成后只 `onComplete()` 关闭弹窗并 `fetch()` 列表。

但 **PlatformSetupQR.tsx**(QR 流) 有显式 restart 按钮(line 259-274)：
```tsx
{t('setup.restartHint', 'Restart the service for the new platform to take effect.')}
<Button onClick={async () => {
  await restartSystem();
  setPhase('restarting' as Phase);
}}>
  <RotateCcw size={14} /> {t('setup.restartNow', 'Restart Now')}
</Button>
```
即 web 前端**把"是否 restart"作为用户决策**，不强制自动重启。

---

## 4. reload 端点行为(是否影响 active sessions) — 重要矛盾

### 4.1 docs 文档承诺(`docs/management-api.md` line 223-239)

```markdown
#### POST /api/v1/reload
Reloads configuration from disk without restarting the process.
**New projects may be added**; removed projects are stopped. Changed project settings take effect.

Response:
{
  "message": "config reloaded",
  "projects_added":   ["new-project"],   // 文档承诺
  "projects_removed": [],                 // 文档承诺
  "projects_updated": ["my-backend"]
}
```

### 4.2 main 分支实际 Go 实现(`core/management.go` line 457-481)

```go
func (m *ManagementServer) handleReload(w http.ResponseWriter, r *http.Request) {
    if r.Method != http.MethodPost { ... }
    m.mu.RLock()
    defer m.mu.RUnlock()

    var updated []string
    for name, e := range m.engines {            // ← 只遍历已有 engine
        if e.configReloadFunc != nil {
            if _, err := e.configReloadFunc(); err != nil { ... }
            updated = append(updated, name)
        }
    }
    mgmtJSON(w, http.StatusOK, map[string]any{
        "message":          "config reloaded",
        "projects_updated": updated,             // ← 只有 updated 字段
    })
}
```

**响应只有 `projects_updated`，没有 `projects_added` / `projects_removed`** —— `web/src/api/status.ts` 的 TypeScript 类型(`projects_added: string[]`)和 docs 文档**领先于实际代码实现**。

### 4.3 reload 实际行为(`cmd/cc-connect/main.go` line 1432-1553)

```go
func reloadConfig(configPath, projName string, engine *core.Engine) (*core.ConfigReloadResult, error) {
    cfg, err := config.Load(configPath)
    if err != nil { return nil, err }
    // 在新 cfg 里查找已知项目
    var proj *config.ProjectConfig
    for i := range cfg.Projects {
        if cfg.Projects[i].Name == projName { proj = &cfg.Projects[i]; break }
    }
    if proj == nil {
        return nil, fmt.Errorf("project %q not found in config", projName)
    }

    // 后续只对 engine 做 setter 调用 — 不创建 engine、不启动平台、不连接 IM
    engine.SetDisplayConfig(...)
    engine.SetShowContextIndicator(...)
    engine.SetReplyFooterEnabled(...)
    engine.SetAutoCompressConfig(...)
    engine.SetResetOnIdle(...)
    engine.SetInstantReply(...)
    engine.SetInjectSender(...)
    engine.SetAttachmentSendEnabled(...)
    engine.SetFilterExternalSessions(...)
    if ps, ok := engine.GetAgent().(core.ProviderSwitcher); ok {
        ps.SetProviders(providers)
        ps.SetActiveProvider(active)
    }
    engine.ClearCommands("config"); engine.AddCommand(...)
    engine.ClearAliases();          engine.AddAlias(...)
    engine.SetBannedWords(...)
    engine.SetDisabledCommands(...)
    engine.SetAdminFrom(...)
    engine.SetUserRoles(...)
    return result, nil
}
```

**结论 — POST /api/v1/reload 当前 main 分支：**
1. **不会启动全新项目** — `handleReload` 只遍历 `m.engines`(已注册的项目 map)，全新 `[[projects]]` 块没对应的 `engine`，被完全忽略
2. **不会停止已删除项目** — 同理
3. **只对已有项目热更新配置**(provider/display/alias/command/banned-words/disabled-commands/admin/users/auto-compress/instant-reply/inject-sender 等)
4. **不会断开 active sessions** — reload 是同步调用 setter，不重启 engine、不断 IM 连接、不影响 PTY；进行中的 chat session 保留状态

### 4.4 restart 端点行为(`core/management.go` line 435-455 + main.go line 1093-1152)

```go
// management.go
func (m *ManagementServer) handleRestart(w http.ResponseWriter, r *http.Request) {
    if r.Method != http.MethodPost { ... }
    var body struct { SessionKey, Platform string }
    json.NewDecoder(r.Body).Decode(&body)  // 可选,用于重启后向该 session 发"重启成功"消息

    select {
    case RestartCh <- RestartRequest{...}:
        mgmtOK(w, "restart initiated")
    default:
        mgmtError(w, 409 /* Conflict */, "restart already in progress")
    }
}

// main.go(主循环 select)
case req := <-core.RestartCh:
    restartReq = &req

slog.Info("shutting down...")
mgmtSrv.Stop()
bridgeSrv.Stop()
webhookSrv.Stop()
heartbeatSched.Stop()
cronSched.Stop()
apiSrv.Stop()
for _, e := range engines {
    e.Stop()                              // ← 关闭所有 engine
}
core.SaveRestartNotify(...)               // 写文件保存 sessionKey,重启后恢复 chat 上下文
execPath, _ := os.Executable()
restartProcess(execPath)                  // ← execve 自身,完整进程重启
```

**结论 — POST /api/v1/restart：**
1. **整个 cc-connect 进程完整重启**(execve 自身 binary)
2. **断开所有 active sessions**(IM 平台 WebSocket 全断、内存 session state 全清空)，重启后从 toml 重新加载所有项目，从存盘的 `RestartNotify` 文件恢复 sessionKey
3. **chat 历史不丢**(session 持久化在 dataDir/sessions/*.json)，但**进行中的 AI turn 会中断**(PTY/HTTP stream 全断)

### 4.5 实操路径推荐(供主线决策)

**新增 platform 到已有项目**：
- `POST /api/v1/projects/{name}/add-platform` → `POST /api/v1/reload`
- reload 不会断 sessions,但**新 platform 不会被启动**(因为 reload 不创建/扩展 platforms,只 setter)
- 实际上**新 platform 仍需要 `POST /api/v1/restart` 才能连上 IM** —— `reloadConfig` 的代码里没有 `engine.AddPlatform` / `engine.RestartPlatforms` 这类调用
- 所以 `restart_required: true` 不是说说而已

**创建全新项目**：
- `POST /api/v1/projects/{name}/add-platform`(任意 name 都行,upsert)
- 必须 `POST /api/v1/restart`：reload 无法启动全新项目
- 重启会断所有 active sessions

**理论上的"零干扰" workaround**：
- 等上游补全 reload 的 `projects_added` 逻辑(docs 已声明,实现未跟上)
- 或在 mini-term 内部规避：只在 cc-connect 还没启动 active session 时创建项目,引导用户在添加项目后再开始用 IM bot
- 或接受"restart 会断 session"作为已知限制(cc-connect web 控制台自身也这样)

---

## 5. 引用

- **management.go(完整路由清单)**: <https://github.com/chenhg5/cc-connect/blob/main/core/management.go#L210-L251>
- **handleProjectRoutes 路由分发**: <https://github.com/chenhg5/cc-connect/blob/main/core/management.go#L575-L633>
- **handleProjectDetail (GET/PATCH/DELETE)**: <https://github.com/chenhg5/cc-connect/blob/main/core/management.go#L635-L825>
- **handleProjects (GET only,无 POST)**: <https://github.com/chenhg5/cc-connect/blob/main/core/management.go#L540-L573>
- **handleProjectAddPlatform**: <https://github.com/chenhg5/cc-connect/blob/main/core/setup.go#L458-L491>
- **handleSetupFeishuSave / WeixinSave**: <https://github.com/chenhg5/cc-connect/blob/main/core/setup.go#L190-L216>, <https://github.com/chenhg5/cc-connect/blob/main/core/setup.go#L428-L454>
- **AddPlatformToProject (upsert)**: <https://github.com/chenhg5/cc-connect/blob/main/config/config.go#L3080-L3124>
- **EnsureProjectWithFeishuPlatform / Weixin**: <https://github.com/chenhg5/cc-connect/blob/main/config/config.go#L1653>, <https://github.com/chenhg5/cc-connect/blob/main/config/config.go#L2031>
- **handleReload (main 分支当前实现)**: <https://github.com/chenhg5/cc-connect/blob/main/core/management.go#L457-L481>
- **handleRestart**: <https://github.com/chenhg5/cc-connect/blob/main/core/management.go#L435-L455>
- **reloadConfig (main.go,只对已存在 engine 做 setter)**: <https://github.com/chenhg5/cc-connect/blob/main/cmd/cc-connect/main.go#L1432-L1553>
- **restartProcess 自重启**: <https://github.com/chenhg5/cc-connect/blob/main/cmd/cc-connect/main.go#L1093-L1152>
- **web/src/api/projects.ts (无 createProject)**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/api/projects.ts>
- **web/src/api/setup.ts (feishu/weixin save)**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/api/setup.ts>
- **web/src/api/status.ts (restartSystem + reloadConfig)**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/api/status.ts>
- **web/src/api/client.ts (响应包装 { ok, data, error })**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/api/client.ts>
- **ProjectList.tsx (add-project wizard)**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/pages/Projects/ProjectList.tsx>
- **PlatformManualForm.tsx (调用 addPlatformToProject)**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/pages/Projects/PlatformManualForm.tsx>
- **PlatformSetupQR.tsx (QR 流 + 显式 restart 按钮)**: <https://github.com/chenhg5/cc-connect/blob/main/web/src/pages/Projects/PlatformSetupQR.tsx>
- **docs/management-api.md (官方文档,reload/restart 描述)**: <https://github.com/chenhg5/cc-connect/blob/main/docs/management-api.md>

## 6. Caveats / Not Found

- `web/src/api/status.ts` 中 `reloadConfig` 的返回类型(`projects_added`/`projects_removed`)和 docs 文档**领先于** Go 实现 — 暗示上游计划支持 reload 热加新项目,但 main 分支 `handleReload` 仍只处理 `m.engines` 里**已注册**的项目。**目前 reload 不能创建/启动新项目**。
- `POST /api/v1/projects/{name}/add-platform` **未列入 docs/management-api.md**,是未文档化的 internal API。可能上游某次 docs 更新会补全。建议在 mini-term 代码注释里明确标注这条端点是从源码反推得到,版本兼容性未保证。
- 没找到全新项目创建后,反过来对已注册项目的影响(比如 dataDir 冲突、agent process 名重复)。这部分需要在真机验证。
- restart 期间 cc-connect 自身的 management API server 也会断,所以 client(mini-term)发完 `POST /restart` 后短时间内拿不到 `GET /status` 响应,需要轮询恢复。restart 通常 1-3 秒完成。

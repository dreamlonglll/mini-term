# Research: cc-connect config.toml 解构 — 字段分类 + UI 控件推荐 + 写回方案

- **Query**: 把 cc-connect config.example.toml（89764B / 1786 行）解构为前端可用的"字段分类 + UI 推荐控件"，并确定是走 cc-connect 自己的 config API 还是自己处理 TOML
- **Scope**: external (cc-connect 上游 main 分支)
- **Date**: 2026-05-28
- **核心结论(给主线 agent)**: cc-connect 自带 **Management API (HTTP :9820)** 已实现 8 大类管理端点(`/projects` `/projects/{name}` `/providers` `/skills` `/cron` `/settings` `/config` `/reload`),足以覆盖 90% 编辑场景且后端保证安全写回。但 `GET /api/v1/config` 仅返回原始 TOML 文本(非 JSON,不脱敏),而 `SaveProjectSettings` 内部走 `saveConfig(cfg) → BurntSushi/toml.Encode → formatTOML` 会**完全丢弃注释**;少数路径(`SaveLanguage`/`patchProjectAgentOption`)用 surgical text edit 保留注释。推荐策略:**优先走 Management API**(reload 不重启 + 后端版本兼容),仅当 API 不覆盖时用 Rust `toml_edit` crate(format-preserving) 落地 mini-term 自己的 TOML 操作。

---

## 顶层 section 分类总表

cc-connect 的 `Config` 结构(`config/config.go` L85-121)定义了完整顶层字段,以下表格按 UI 编辑频率分类:

| Section | TOML 类型 | UI 频率 | 说明 |
|---|---|---|---|
| (顶层) language / data_dir / attachment_send / banned_words / quiet / idle_timeout_mins / max_turn_time_mins / workspace_idle_timeout_mins / provider_presets_url | scalar | 中 | 全局开关 |
| `[log]` | table | 低 | level: debug/info/warn/error |
| `[display]` | table | 中 | thinking/tool 消息显示策略 |
| `[stream_preview]` | table | 低 | 实时流式预览节流 |
| `[instant_reply]` | table | 低 | 即时确认回复 |
| `[rate_limit]` | table | 低 | 入站速率限制 |
| `[outgoing_rate_limit]` + `.platforms.*` | table + 嵌套 map | 低 | 出站节流(防封号) |
| `[relay]` | table | 极低 | bot-to-bot relay 超时 |
| `[cron]` | table | 低 | 定时任务全局默认 |
| `[queue]` | table | 极低 | per-session 消息队列 |
| `[webhook]` | table | 低 | 外部 HTTP webhook (`:9111`) |
| `[bridge]` | table | 低 | 外部适配器 WebSocket (`:9810`) |
| `[management]` | table | 低 | **Management API 自身配置** (`:9820`) |
| `[speech]` + `.openai/.groq/.qwen/.gemini` | table | 极低 | 语音转文字 |
| `[tts]` + `.openai/.qwen/.minimax/.mimo` | table | 极低 | 文字转语音 |
| `[[providers]]` | 表数组 | **高** | 全局复用的 API Provider |
| `[[hooks]]` | 表数组 | 低 | 事件钩子(command/http) |
| `[[commands]]` | 表数组 | 中 | 全局自定义 slash 命令 |
| `[[aliases]]` | 表数组 | 低 | 命令别名 |
| `[[projects]]` | 表数组 | **极高** | 项目数组(每个项目一棵嵌套子树) |
| `[[projects.agent]]` / `[projects.agent.options]` / `.env` | nested | **极高** | agent 类型 + work_dir + mode + 环境变量 |
| `[[projects.agent.providers]]` + `.models` | 表数组嵌套 | **高** | 项目级 provider(可单独于全局) |
| `[[projects.platforms]]` + `.options` | 表数组嵌套 | **高** | 项目绑定的 IM 平台 |
| `[projects.heartbeat]` | nested table | 低 | 项目心跳 |
| `[projects.auto_compress]` | nested table | 低 | 自动 /compress |
| `[projects.observe]` | nested table | 极低 | 终端会话转发到 IM |
| `[projects.references]` | nested table | 极低 | 文件引用标准化 |
| `[projects.display]` | nested table | 低 | per-project 覆盖全局 display |
| `[projects.users]` + `.roles.*` | nested + map | 低 | 多用户角色 ACL |

**注**:任务卡里提到的 `[redact]`(密文显示)、`[skill]`(独立 section)在 config.toml 中**并不存在**;
- "密文显示"是前端职责,后端通过 `GET /api/v1/projects` 返回的字段命名(`api_key`/`token`/`app_secret`/`client_secret`)隐式标记;
- "Skill"在 config.toml 中实际就是 `[[commands]]`,但 Management API 另有独立 `/api/v1/skills` 端点(可能管理 Claude Code skills 目录中的 `.md` 文件,非 TOML 字段)。

---

## 每 section 字段表(含敏感字段标注)

> **图例**: 🔒=敏感(密文显示) / 🔑=必填 / ⭐=高频 / ⚡=高级

### 顶层全局

| name | type | default | sensitive | category | UI 控件 |
|---|---|---|---|---|---|
| `language` | enum: ""/"en"/"zh"/"zh-TW"/"ja"/"es" | "" (自动) | | 高频 | Select |
| `data_dir` | string (path) | `~/.cc-connect` | | 高级 | FolderPicker |
| `attachment_send` | enum: "on"/"off" | "on" | | 低频 | Switch |
| `banned_words` | array of string | [] | | 低频 | TagInput |
| `quiet` | bool (legacy) | false | | 低频 | Switch (隐藏在 Advanced) |
| `idle_timeout_mins` | int (mins, 0 = 禁用) | 120 | | 低频 | NumberInput |
| `max_turn_time_mins` | int (mins, 0 = 禁用) | 0 | | 高级 | NumberInput |
| `workspace_idle_timeout_mins` | int | 15 | | 高级 | NumberInput |
| `provider_presets_url` | string (URL) | github URL | | 高级 | TextInput |

### `[log]`

| name | type | default | sensitive | UI 控件 |
|---|---|---|---|---|
| `level` | enum: "debug"/"info"/"warn"/"error" | "info" | | Select |

### `[display]` (中频)

| name | type | default | UI 控件 |
|---|---|---|---|
| `mode` | enum: "full"/"compact"/"quiet" | "full" | RadioGroup |
| `card_mode` | enum: "legacy"/"rich" | "legacy" | RadioGroup |
| `thinking_messages` | bool | true | Switch |
| `thinking_max_len` | int (0 = 不截断) | 300 | NumberInput |
| `tool_messages` | bool | true | Switch |
| `tool_max_len` | int (0 = 不截断) | 500 | NumberInput |
| `show_context_indicator` | bool | true | Switch |
| `reply_footer` | bool | true | Switch |

### `[stream_preview]` / `[instant_reply]` / `[rate_limit]` / `[outgoing_rate_limit]` / `[relay]` / `[cron]` / `[queue]`

(略 - 全是 bool/int 字段,UI 控件均为 Switch/NumberInput)

### `[webhook]` (服务端口配置)

| name | type | default | sensitive | UI 控件 |
|---|---|---|---|---|
| `enabled` | bool | false | | Switch |
| `port` | int | 9111 | | NumberInput |
| `token` | string | "" (= 不认证) | 🔒 | PasswordInput |
| `path` | string | "/hook" | | TextInput |

### `[bridge]`

| name | type | default | sensitive | UI 控件 |
|---|---|---|---|---|
| `enabled` | bool | false | | Switch |
| `port` | int | 9810 | | NumberInput |
| `token` | string | (必填) | 🔒 🔑 | PasswordInput |
| `path` | string | "/bridge/ws" | | TextInput |
| `cors_origins` | array of string | [] | | TagInput |
| `insecure` | bool | false | | Switch (危险,加 ⚠) |

### `[management]` (本面板自己的依赖,UI 应该警告"修改后需要重启")

| name | type | default | sensitive | UI 控件 |
|---|---|---|---|---|
| `enabled` | bool | false | | Switch |
| `port` | int | 9820 | | NumberInput |
| `token` | string | (必填) | 🔒 🔑 | PasswordInput |
| `cors_origins` | array of string | [] | | TagInput |

### `[speech]` / `[tts]` 子表

每个子 provider(openai/groq/qwen/gemini/minimax/mimo)都有:

| name | type | sensitive | UI 控件 |
|---|---|---|---|
| `api_key` | string | 🔒 🔑 | PasswordInput |
| `base_url` | string | | TextInput |
| `model` | string | | Select(预填模型列表)/TextInput |

### `[[providers]]` (全局 provider,高频)

| name | type | sensitive | UI 控件 |
|---|---|---|---|
| `name` | string | 🔑 | TextInput (作为引用 key) |
| `api_key` | string | 🔒 🔑 | PasswordInput |
| `base_url` | string | | TextInput |
| `model` | string | | TextInput |
| `models` | array of `{model, alias}` | | RepeatableForm |
| `thinking` | enum: "disabled"/(空) | | RadioGroup |
| `env` | map<string, string> | 部分 🔒 | KeyValueEditor |
| `agent_types` | array of enum: "claudecode"/"codex"/... | | MultiSelect |
| `endpoints` / `agent_models` / `agent_model_lists` | map | | 高级嵌套表单 |
| `codex.env_key` / `codex.wire_api` / `codex.http_headers` | nested | 部分 🔒 | 仅 codex 显示 |

### `[[hooks]]`

| name | type | UI 控件 |
|---|---|---|
| `event` | enum: 8 种事件 + "*" | Select |
| `type` | enum: "command"/"http" | RadioGroup |
| `command` (type=command 时) | string | Textarea |
| `url` (type=http 时) | string | TextInput |
| `timeout` | int (seconds) | NumberInput |
| `async` | bool | Switch |

### `[[commands]]`

| name | type | UI 控件 |
|---|---|---|
| `name` | string | TextInput |
| `description` | string | TextInput |
| `prompt` (prompt 命令) | string with `{{1}}`/`{{2*}}`/`{{args}}` 占位符 | Textarea |
| `exec` (exec 命令) | string | TextInput |
| `work_dir` | string (path) | FolderPicker |

`prompt` 和 `exec` 互斥(UI 用 RadioGroup 切换形态)。

### `[[projects]]` (核心数组,极高频)

这是配置文件最复杂的部分,UI 必须做"项目列表 + 项目编辑器"两栏:

| name | type | sensitive | category | UI 控件 |
|---|---|---|---|---|
| `name` | string | 🔑 | 高频 | TextInput |
| `mode` | enum: ""/"multi-workspace" | | 低频 | Select |
| `base_dir` (multi-workspace) | string (path) | | 低频 | FolderPicker |
| `skip_git` | bool | | 低频 | Switch |
| `workspace_init_allow_local_paths` | bool | | 高级 | Switch |
| `run_as_user` | string (Linux/macOS only) | | 高级 | TextInput (Windows 隐藏) |
| `run_as_env` | array of string | | 高级 | TagInput |
| `show_context_indicator` | bool | | 低频 | Switch |
| `reply_footer` | bool | | 低频 | Switch |
| `inject_sender` | bool | | 低频 | Switch |
| `disabled_commands` | array of string | | 低频 | TagInput |
| `admin_from` | string ("\*" 或 "id,id") | | 低频 | TextInput (⚠ 风险提示) |
| `filter_external_sessions` | bool | | 低频 | Switch |
| `reset_on_idle_mins` | int (mins, 0 = 禁用) | | 低频 | NumberInput |
| `quiet` (legacy) | bool | | 低频 | Switch (Advanced) |

#### `[projects.agent]` (极高频)

| name | type | sensitive | UI 控件 |
|---|---|---|---|
| `type` | enum: claudecode/codex/cursoragent/gemini/qoder/opencode/devin/acp/iflow/kimi/tmux | 🔑 | Select |
| `options.work_dir` | string (path) | 🔑 | FolderPicker |
| `options.mode` | enum: default/acceptEdits/plan/auto/bypassPermissions/dontAsk | | RadioGroup |
| `options.reasoning_effort` | enum: low/medium/high/max | | RadioGroup |
| `options.allowed_tools` | array of string | | TagInput(预设候选 Read/Grep/Bash/...) |
| `options.disallowed_tools` | array of string | | TagInput |
| `options.system_prompt` | string | | Textarea |
| `options.model` | string | | TextInput |
| `options.router_url` / `options.router_api_key` | string | 部分 🔒 | TextInput / PasswordInput |
| `options.env` | map<string, string> | 部分 🔒 | KeyValueEditor |
| `provider` | string (active provider name) | | Select(从 providers 候选) |
| `provider_refs` | array of string | | MultiSelect(全局 providers) |
| `providers[]` | 同 [[providers]] | 含 🔒 | RepeatableForm |

#### `[[projects.platforms]]` (高频,枚举繁多)

每个平台都有不同 options。已支持平台(从 config.example.toml 提取):

`feishu` / `lark` / `dingtalk` / `wps-xiezuo` / `telegram` / `max` / `slack` / `discord` / `wecom`(企业微信) / `weixin`(微信公众号) / `whatsapp` / `qq` / `webhook`(通用)等

通用字段:`type`(枚举,Select), `allow_from`(string), `allow_chat`(string), `group_only`(bool), `share_session_in_channel`(bool), `reply_to_trigger`(bool)

敏感字段(平台凭证):
- `app_id` / `app_secret` (feishu/lark/wps-xiezuo) — `app_secret` 🔒
- `token` (telegram/max/discord/webhook) 🔒
- `bot_token` / `app_token` (slack) 🔒
- `client_id` / `client_secret` (dingtalk) — `client_secret` 🔒
- `webhook_secret` (max webhook 模式) 🔒
- `encrypt_key` (lark webhook 模式) 🔒

UI 应该按 `type` 字段动态切换 options 表单(类似 JSON Schema oneOf)。

#### `[projects.heartbeat]` / `[projects.auto_compress]` / `[projects.observe]` / `[projects.references]` / `[projects.display]`

参数都已在前面的全局表中类似,这里是 per-project 覆盖。

#### `[projects.users]` + `[projects.users.roles.<role>]`

| name | type | UI 控件 |
|---|---|---|
| `default_role` | string | TextInput |
| `roles.<name>.user_ids` | array of string("\*"=匹配剩余) | TagInput |
| `roles.<name>.disabled_commands` | array of string | TagInput |
| `roles.<name>.rate_limit.max_messages` | int | NumberInput |
| `roles.<name>.rate_limit.window_secs` | int | NumberInput |

---

## 运行时 reload vs 重启

| 操作 | 生效方式 |
|---|---|
| `POST /api/v1/reload` | 后端遍历 `engines[].configReloadFunc`,重新读 config.toml,**不重启进程**,新项目可加入,已删除项目停止,变更字段生效 |
| 修改 `[management]` 自身 (port/token) | 必须重启 cc-connect 进程,Management API 服务监听不会 hot-rebind |
| 修改 `[projects.agent].type` | API 返回 `restart_required: true`,前端必须提示用户手动 POST `/restart` |
| 修改 `[bridge]` (port/token) | 类似 management,需要重启 |
| 其余字段 (provider/platform/heartbeat/cron/...) | `POST /reload` 即可生效 |

---

## 写回方案对比

### 方案 A: 走 cc-connect Management API (REST,已实现)

**端点清单**(`core/management.go` L228-249):
- `GET /api/v1/status` — 系统状态
- `POST /api/v1/restart` — 优雅重启
- `POST /api/v1/reload` — 配置 hot-reload
- `GET /api/v1/config` — **返回 raw TOML 文本(text/plain,非 JSON,不脱敏)**
- `GET /api/v1/settings` / `PATCH /api/v1/settings` — 全局设置(JSON 增量更新)
- `GET /api/v1/agents` — 列出已注册 agent 类型
- `GET /api/v1/projects` / `GET /api/v1/projects/{name}` / `PATCH .../{name}` / `DELETE .../{name}` — 项目 CRUD
- `POST /api/v1/projects/{name}/add-platform` — 添加平台
- `GET/POST/DELETE /api/v1/projects/{name}/sessions[...]` — 会话管理
- `POST /api/v1/projects/{name}/send` — 发消息到 session
- `GET/POST/DELETE /api/v1/projects/{name}/providers[...]` / `.../activate` — 项目 provider CRUD
- `POST /api/v1/projects/{name}/provider-refs` — 引用全局 provider
- `GET /api/v1/projects/{name}/models` / `POST /api/v1/projects/{name}/model` — 模型管理
- `GET/POST /api/v1/projects/{name}/heartbeat[/pause/resume/run/interval]`
- `GET/PATCH /api/v1/projects/{name}/users` — 用户角色
- `GET /api/v1/providers` / `POST` / `PATCH/DELETE /api/v1/providers/{name}` — 全局 provider CRUD
- `GET/POST/DELETE /api/v1/cron[/...] ` — cron CRUD
- `POST /api/v1/setup/feishu/{begin,poll,save}` / `setup/weixin/{begin,poll,save}` — 平台配置向导(支持二维码扫描登录)
- `GET /api/v1/skills` / `GET /api/v1/skills/presets`
- `GET /api/v1/bridge/adapters`

**认证**: `Authorization: Bearer <token>` 或 `?token=` query,token 来自 `[management].token`,常量时间比较防侧信道。
**CORS**: `[management].cors_origins`,需要显式列出 mini-term 的 origin(或设为 `["*"]`)。
**响应格式**: `{ok: true, data: ...}` / `{ok: false, error: ...}`。

**优点**:
- 后端版本兼容性自动保证 — 我们不需要追 toml schema 变化
- 写回触发 hot-reload(reload + saveProject 在同一请求内)无需重启
- 平台凭证向导(feishu/weixin scan/poll/save)直接复用,省一周工作量
- 服务端 `saveProjectSettings` 已带"修改 agent.type 需要重启"等业务规则

**缺点**:
- `GET /api/v1/config` **只返回原始 TOML 字符串**,且**不脱敏 secret**(spec 写要脱敏,代码实现没做),前端要么自己渲染原文(给"高级编辑器"用)、要么从其他端点拼装表单数据
- `PATCH /api/v1/projects/{name}` 只覆盖固定的 11 个字段(`Language`/`AdminFrom`/`DisabledCommands`/`WorkDir`/`Mode`/`AgentType`/`ShowContextIndicator`/`ReplyFooter`/`InjectSender`/`PlatformAllowFrom`/`agent` 子字段),其余字段(如 `[projects.heartbeat]`/`[projects.references]`/`[projects.observe]`/`reset_on_idle_mins`等)**没有 API 入口**
- `saveProjectSettings` → `saveConfig(cfg)` 内部用 BurntSushi/toml.Encode 序列化整个 Config 结构,然后 `formatTOML` 重排空行 —— **完全丢弃用户原 TOML 中的所有注释**(只有 `SaveLanguage`/`patchProjectAgentOption`/`patchTopLevelField` 等少数 helper 走 surgical text edit)
- 需要 cc-connect 进程在运行且 `[management].enabled = true`、token 已配置 —— 如果用户刚装上还没启动,不能通过 API 写初始配置;需要 fallback 路径

### 方案 B: mini-term 后端(Rust)用 `toml_edit` crate 直接读写 config.toml

**`toml_edit` crate**:
- 仓库: github.com/toml-rs/toml (官方 toml-rs 生态)
- 当前版本: 0.25.x(2026-05)
- 标语: "Yet another **format-preserving** TOML parser" — 显式保证 "preserving comments, spaces and relative order of items"
- 唯一已知限制: 不保证 dotted-key 顺序(`a.b.c = 1` 的子 key 顺序),可忽略
- 用法: `let mut doc = toml.parse::<DocumentMut>()?; doc["projects"][0]["agent"]["options"]["work_dir"] = value("...")` 即可,`doc.to_string()` 重建 TOML 原样保留注释

**优点**:
- 编辑后保留全部中英文注释(`# Display Settings / 显示设置` 这种重要说明不会丢)
- 不依赖 cc-connect 进程运行 — 可以在 cc-connect 没启动时也提供"先编辑、待会儿启动"流程
- mini-term 自己掌握 schema 演进节奏

**缺点**:
- 需要在 Rust 端复现 cc-connect 的 schema 知识(每次 cc-connect 升级 toml 字段都要跟版本)
- 业务规则不能复用(如"修改 agent.type 需要 restart"逻辑、provider_refs 兼容性过滤)
- 写完之后还得自己 POST `/api/v1/reload` 让 cc-connect 重新加载

### 方案 C: 混合(推荐)

- **读**: 优先 `GET /api/v1/projects`、`/providers`、`/settings`、`/cron`、`/skills` 这些**已经返回 JSON 结构化数据**的端点(没有脱敏问题,后端自动按 schema 给字段)
- **写常见字段**: 走对应 PATCH/POST 端点,享受业务规则 + hot reload
- **写 API 未覆盖的字段**(heartbeat/observe/references/auto_compress 等): 用 Rust `toml_edit` 直接改 config.toml,然后 POST `/api/v1/reload` 触发 cc-connect 重新加载
- **首次配置(cc-connect 没启动时)**: 完全走 `toml_edit` 写文件,提示用户启动 cc-connect
- **`config.toml` 原文查看/导入/导出**: 用 `GET /api/v1/config` 拉文本展示 + 走 `toml_edit` 解析渲染 form,或者直接给"高级编辑器"显示原文

---

## 推荐策略

### 1. UI 整体结构

```
左侧导航(树形或 Tab):
├── 全局
│   ├── 基础(language/data_dir/banned_words)
│   ├── 显示(display/stream_preview/instant_reply)
│   ├── 速率(rate_limit/outgoing_rate_limit/queue)
│   ├── 集成(webhook/bridge/management)
│   ├── 语音(speech/tts)
│   └── 高级(idle_timeout_mins/max_turn_time/quiet/cron 默认)
├── 全局 Providers (table)
├── Hooks (table)
├── Commands & Aliases (table)
├── 项目(列表 + "+" 新建)
│   └── 单项目编辑器(右栏)
│       ├── 基本(name/run_as_user/admin_from)
│       ├── Agent (type 切换决定 options 子表单)
│       ├── Providers (列表)
│       ├── Platforms (列表 + type 切换决定 options 子表单)
│       ├── Heartbeat / AutoCompress / Observe
│       ├── Display 覆盖
│       └── Users & Roles
└── 高级 / 原始 TOML 编辑
```

### 2. 数据来源策略

| UI 区域 | 读数据 | 写数据 |
|---|---|---|
| 全局基础/显示/速率 | `GET /api/v1/settings` | `PATCH /api/v1/settings` |
| 全局 Providers | `GET /api/v1/providers` | `POST`/`PATCH`/`DELETE /api/v1/providers/...` |
| 项目列表 | `GET /api/v1/projects` | (列表是只读) |
| 项目详情(常见字段) | `GET /api/v1/projects/{name}` | `PATCH /api/v1/projects/{name}` |
| 项目 Providers | `GET .../providers` | `POST/DELETE/POST .../activate` |
| 项目 Platforms 添加 | (本地 form) | `POST /api/v1/projects/{name}/add-platform` |
| 项目 Heartbeat | `GET .../heartbeat` | `POST .../heartbeat/{pause,resume,run,interval}` |
| 项目 Users | `GET .../users` | `PATCH .../users` |
| Cron | `GET /api/v1/cron` | `POST`/`DELETE /api/v1/cron[/...]` |
| Skills | `GET /api/v1/skills` | (Skills 接口仅 GET,详查实现) |
| 项目 References / Observe / AutoCompress / display 覆盖 / mode=multi-workspace 字段 | `toml_edit` 解析原文 | `toml_edit` 写回 + `POST /api/v1/reload` |
| 平台 setup 向导(扫码登录) | `POST /api/v1/setup/feishu/begin` 等 | 端到端走 API |

### 3. 敏感字段密文显示

前端通过字段名 heuristic 标记:`api_key` / `token` / `secret` / `password` / `webhook_secret` / `encrypt_key` / `client_secret` / `app_secret` / `bot_token` / `app_token` → 一律渲染 PasswordInput(value="·····",点击"显示"才明文)。

由于 `GET /api/v1/config` 返回原文未脱敏,而 `GET /api/v1/projects/{name}` 等 JSON 端点会原样返回 secret 值,**前端不要把这些字段写到 UI store 持久层(localStorage / IndexedDB),仅放内存**;mini-term 自身的 Tauri secure store 应该用 OS keychain 而不是普通文件。

### 4. 写回保护

- 每次写回前先做 toml 语法验证(用 `toml_edit::DocumentMut::parse` 试解析)
- 写回时先写到 `config.toml.tmp` 然后 atomic rename(cc-connect 后端的 `saveConfig` / `FormatConfigFile` 已经这样做,自己实现要复刻)
- 用 file watcher 监听 config.toml,如果外部修改则提示用户冲突
- 保留最近 N 个备份(`config.toml.bak.<ts>`)

---

## 风险点

1. **`GET /api/v1/config` 不脱敏**: spec 说脱敏、实现没做。如果 mini-term 跨网络暴露这个 endpoint(管 cc-connect 远程实例)而 cors_origins=`*`,会泄漏所有 secret。**对策**: 默认只允许 `127.0.0.1` cc-connect,远程一律警告。
2. **注释丢失**: `PATCH /api/v1/projects/{name}` 走 `saveProjectSettings → saveConfig → toml.Encode`,会**清空 config.toml 内所有用户注释**。如果用户在 config.toml 里写了自己的备忘注释,一次保存就丢光。**对策**: 第一次走 API 写之前用 `toml_edit` 在 Rust 端备份原文,或者干脆不用这个 PATCH 端点,走 toml_edit 直接改 + reload。
3. **API spec 与实现脱节**: `docs/management-api.md` 第一行写 "Draft — subject to change before implementation",一些端点(如 `GET /api/v1/logs`)在 spec 中存在但在 `core/management.go` 中并未注册(实测只有 status/restart/reload/config/settings/agents/projects/providers/skills/bridge/cron 这几大类)。**对策**: 以 `core/management.go` 中实际 `mux.HandleFunc` 为准,spec 仅作参考。
4. **cc-connect 版本兼容**: schema 还在快速迭代(CHANGELOG 接近 50KB);mini-term 应该把 cc-connect 版本号 pin 到一个 minor,启动时拉 `/api/v1/status.version` 验证兼容范围。
5. **Windows 平台限制**: `run_as_user` 字段在 Windows 上后端会硬报错(`validateRunAsUser` 拒绝),UI 在 Windows 上要灰掉这个 fieldset。
6. **`token = ""` 即免认证**: management.go 的 `authenticate` 在 `m.token == ""` 时直接 return true,允许任何请求通过。如果用户不小心把 token 字段清空,Management API 立刻完全裸奔。UI 应该在保存空 token 时弹强警告。
7. **`[management].insecure` 字段**: 我们查到 `BridgeConfig` 有 `insecure` 字段允许跳过 token 校验(only 本地 dev),`ManagementConfig` 当前**没有**这个字段,但任何时候 cc-connect 加上类似字段,前端要同步加危险提示。
8. **TOML 字符串 `${VAR}` 替换**: 所有字符串值支持 `${ENV_VAR}` 替换。UI 显示原值时要保留 `${}` 占位符,不要展开为实际环境变量值(否则会把 secret 印到前端)。

---

## 引用

### cc-connect 上游代码(主分支,2026-05-28 抓取)
- `config.example.toml` (89764B, 1786 行) — 完整带注释样例
  https://raw.githubusercontent.com/chenhg5/cc-connect/main/config.example.toml
- `config/config.go` (108244B, ~3430 行) — Config struct 定义 + Save/Format/Patch 实现
  https://raw.githubusercontent.com/chenhg5/cc-connect/main/config/config.go
- `cmd/cc-connect/config_cmd.go` — `cc-connect config example|format|fmt|path` 子命令
  https://raw.githubusercontent.com/chenhg5/cc-connect/main/cmd/cc-connect/config_cmd.go
- `core/management.go` (~1950 行) — Management API 实际实现,所有路由注册
  https://raw.githubusercontent.com/chenhg5/cc-connect/main/core/management.go
- `core/api.go` — Unix socket API(本地 /send /sessions /cron /relay)
  https://raw.githubusercontent.com/chenhg5/cc-connect/main/core/api.go
- `docs/management-api.md` (28KB) — REST API spec(标记 Draft,与实现略有出入)
  https://github.com/chenhg5/cc-connect/blob/main/docs/management-api.md
- `web/embed.go` — cc-connect 自身已经有 web 管理面板,通过 `//go:embed all:dist` 嵌入 SPA
  https://github.com/chenhg5/cc-connect/blob/main/web/embed.go

### Rust 端 TOML 写回相关
- `toml_edit` crate (toml-rs 官方,format-preserving)
  - https://crates.io/crates/toml_edit (latest: 0.25.12+spec-1.1.0)
  - https://github.com/toml-rs/toml/tree/main/crates/toml_edit
  - README: "preserving comments, spaces and relative order of items"
- 备选(纯 Rust): `serde-toml` / `toml-rs basic` — 不保留注释,不推荐
- Go 生态对比(cc-connect 后端用):BurntSushi/toml(`saveConfig`)不保留注释,所以官方写回也会清注释

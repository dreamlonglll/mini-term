# Research: cc-connect web 前端实现模式 + iframe 嵌入可行性

- **Query**: 嵌入 cc-connect 控制面板到 mini-term —— 调研 cc-connect/web/ 前端实现 + iframe 嵌入可行性
- **Scope**: external (chenhg5/cc-connect GitHub repo, main 分支)
- **Date**: 2026-05-28
- **上游 commit**: pushed_at 2026-05-28T02:07:57Z

> 重要前置:cc-connect 本身是一个 Go 二进制，主进程 `cc-connect` 跑 daemon + 内嵌 HTTP server (端口默认 9820)，`cc-connect web` 子命令**只负责生成/读取 token 并打开浏览器**，不启 HTTP 服务。web 前端代码在 `web/` 目录,通过 `web/embed.go` 的 `//go:embed all:dist` 把 `npm run build` 产物打进 Go 二进制,运行时由 `core.ManagementServer` (在 `core/management.go`) 静态挂载到 `/`，API 走 `/api/v1/*`，bridge WebSocket 走 `/bridge` (实际 path 由 server 配置)。

---

## 技术栈

源:`web/package.json`

| 维度 | 选型 | 版本 |
|---|---|---|
| 框架 | React | 19.1.0 |
| 类型 | TypeScript | 5.8.3 (strict) |
| 构建 | Vite | 6.3.2 |
| 路由 | react-router-dom | 7.5.0 (BrowserRouter, 非 Hash) |
| 状态管理 | **zustand** | 5.0.5 |
| 网络 | **原生 fetch** (无 axios / 无 react-query / 无 swr) | — |
| 样式 | **Tailwind CSS v3.4.17** (传统配置,**非 v4 zero-config**) | 3.4.17 |
| Tailwind 插件 | `@tailwindcss/typography` (用于 markdown 渲染) | 0.5.19 |
| 图标 | lucide-react | 0.487.0 |
| Markdown | react-markdown + remark-gfm + rehype-highlight + highlight.js | 10.1.0 / 4.0.1 / 7.0.2 / 11.11.1 |
| 二维码 | qrcode.react | 4.2.0 |
| i18n | i18next + react-i18next | 25.1.2 / 15.5.1 |
| 包管理 | pnpm (有 pnpm-workspace.yaml + pnpm-lock.yaml) | — |
| 颜色变量 | CSS Custom Property `--color-accent` + Tailwind `rgb(var(...) / <alpha-value>)`,深浅主题切 `.dark` class | — |

注意:**没有用 shadcn/ui、Radix、Mantine、Ant Design 等组件库**。所有基础组件 (Button / Card / Badge / Input / Modal / EmptyState) 都是 `web/src/components/ui/` 下自己写的薄壳,~1-2KB 一个。

---

## 目录结构

```
web/
├── package.json           # 上方技术栈
├── vite.config.ts         # dev port 9821, proxy /api→9820 /bridge→9810
├── tailwind.config.ts     # darkMode: 'class', content: ./src
├── tsconfig.json          # paths: @/* → src/*
├── index.html             # 单 <div id="root">,挂 /src/main.tsx
├── embed.go               # //go:embed all:dist + core.RegisterWebAssets
├── embed_stub.go          # //go:build no_web 兜底,可关 web
├── dist/                  # vite build 产物 (.gitignore'd,运行时被 embed)
└── src/
    ├── main.tsx           # BrowserRouter + 调 useAuthStore.init / useThemeStore.init
    ├── App.tsx            # Routes 定义 (见下"路由与布局")
    ├── index.css          # @tailwind base/components/utilities + highlight.js 主题
    ├── api/               # 全部 12 个 API 模块,见"API 调用模式"
    ├── components/
    │   ├── Layout/        # Layout.tsx / Sidebar.tsx / Header.tsx / Footer.tsx
    │   └── ui/            # Button/Card/Badge/Input/Modal/EmptyState
    ├── hooks/
    │   └── useBridgeSocket.ts  # WebSocket 长连接,接 IM 消息流
    ├── i18n/              # 5 语言 (en/zh/zh-TW/ja/es)
    ├── lib/
    │   ├── platformMeta.ts
    │   └── utils.ts       # cn(), formatUptime, formatTime
    ├── pages/
    │   ├── Dashboard.tsx       # 7.5KB,统计 + 最近 session
    │   ├── Login.tsx           # 7.2KB,token 输入 + /login?token= 自动登录
    │   ├── Chat/
    │   │   ├── ChatList.tsx           # 5.6KB,项目列表
    │   │   ├── ChatView.tsx           # 32KB ★ IM 主聊天界面
    │   │   ├── CommandPalette.tsx     # 7.7KB,/ 命令面板
    │   │   ├── CommandResultPanel.tsx # 8.7KB,命令结果侧栏
    │   │   └── SessionDrawer.tsx      # 5.2KB,会话切换抽屉
    │   ├── Sessions/                  # 旧式 session 列表 (项目级)
    │   │   ├── SessionList.tsx
    │   │   └── SessionChat.tsx        # 25KB,旧聊天页 (project 内)
    │   ├── Projects/
    │   │   ├── ProjectList.tsx        # 11.8KB
    │   │   ├── ProjectDetail.tsx      # 35.9KB ★ 项目配置主页
    │   │   ├── PlatformSetupQR.tsx    # 11.9KB,二维码绑定流程
    │   │   └── PlatformManualForm.tsx # 5.7KB,手动表单
    │   ├── Providers/                 # 全局 LLM provider 管理
    │   ├── Skills/                    # 技能(skills) 浏览
    │   ├── Cron/                      # cron 任务
    │   ├── Bridge/                    # bridge adapter 状态
    │   └── System/
    │       ├── Config.tsx             # 4.1KB,只读 raw config TOML/JSON
    │       └── GlobalSettings.tsx     # 10.5KB ★ 结构化表单
    └── store/
        ├── auth.ts        # token 存 localStorage('cc_token'),Bearer Auth
        └── theme.ts       # light/dark/system,挂 document.documentElement.classList
```

### 关键文件 → 功能映射

| 用户问题对应 | 主要文件 | 备注 |
|---|---|---|
| sessions 列表 | `pages/Sessions/SessionList.tsx`, `pages/Chat/SessionDrawer.tsx` | 抽屉式切换 |
| messages (IM 流) | `pages/Chat/ChatView.tsx` (32KB,**最重要**) | bridge WS + markdown |
| config 编辑 | `pages/System/Config.tsx` + `pages/System/GlobalSettings.tsx` | **不是 Monaco;表单 + 只读 raw pre** |
| cron | `pages/Cron/CronList.tsx` (没单独看,但 `api/cron.ts` 810 字节,小) | — |
| relay/bridge | `pages/Bridge/`, `api/bridge.ts`, `hooks/useBridgeSocket.ts` | 见"消息流"章节 |
| providers | `pages/Providers/ProviderList.tsx`, `api/providers.ts` | 含 cc-switch 迁移导入 |
| dashboard | `pages/Dashboard.tsx` | 4 个 StatCard + projects/sessions 网格 |

---

## API 调用模式

### 客户端封装:`web/src/api/client.ts`

整段代码 (略去注释,纯 fetch,共 80 行,**核心**):

```ts
const API_BASE = '/api/v1';   // ★ 相对路径,不带 host

class ApiClient {
  private token: string = '';

  setToken(token: string) { this.token = token; }
  setOnUnauthorized(handler: () => void) { ... }

  private headers(): HeadersInit {
    const h: HeadersInit = { 'Content-Type': 'application/json' };
    if (this.token) h['Authorization'] = `Bearer ${this.token}`;
    return h;
  }

  async request<T>(method, path, body?, params?) {
    let url = `${API_BASE}${path}`;
    if (params) url += `?${new URLSearchParams(params)}`;
    const res = await fetch(url, { method, headers: this.headers(), body: body ? JSON.stringify(body) : undefined });
    if (res.status === 401) this.onUnauthorized?.();
    const json = await res.json();
    if (!json.ok) throw new ApiError(json.error, res.status);
    return json.data as T;
  }

  get<T>(path, params?) { return this.request<T>('GET', path, undefined, params); }
  post / put / patch / delete<T>(path, body?) { ... }

  /** Raw text (非 JSON) — 用于 GET /config 拉 TOML */
  async raw(path: string): Promise<string> { ... }
}

export const api = new ApiClient();
```

**核心结论**:
- **用原生 fetch**,完全没有 axios
- **base URL 是相对路径** `/api/v1` — 关键:依赖 Vite dev server 把 `/api` 反代到 `http://localhost:9820`,**生产环境则依赖 web 前端与 management API 同源** (因为它是被 Go 二进制内嵌的)
- **Auth 通过 `Authorization: Bearer <token>` header**
- **Token 持久化在 localStorage `cc_token`** (`store/auth.ts`)
- **响应包络**: `{ ok: true, data: T }` 或 `{ ok: false, error: string }`
- **401 → 触发 logout** (在 `main.tsx` 注册了 `api.setOnUnauthorized(() => useAuthStore.getState().logout())`)
- **没有 react-query / SWR**,每个组件自己 `useEffect + setState` 拉数据;有个全局 `window.dispatchEvent(new CustomEvent('cc:refresh'))` 用于"刷新"按钮触发各页 reload (Header.tsx → 各页 useEffect 监听)

### API 模块清单 (`web/src/api/`)

| 文件 | size | 主要 endpoint |
|---|---|---|
| `client.ts` | 2.4KB | fetch 封装 |
| `index.ts` | 234B | 桶导出 |
| `status.ts` | 0.6KB | `GET /status` → 含 bridge port/path/token |
| `projects.ts` | 2.0KB | CRUD /projects, /projects/{name}, /add-platform |
| `sessions.ts` | 1.5KB | /projects/{p}/sessions, /sessions/{id}, /send |
| `providers.ts` | 3.5KB | 项目+全局 provider,cc-switch 迁移 |
| `cron.ts` | 0.8KB | cron 任务 CRUD |
| `heartbeat.ts` | 0.9KB | /projects/{p}/heartbeat/{pause,resume,run,interval} |
| `settings.ts` | 0.6KB | `GET/PATCH /settings` (全局开关,结构化) |
| `setup.ts` | 1.8KB | feishu/weixin QR 引导流程 |
| `skills.ts` | 1.0KB | 技能列表/预设 |
| `bridge.ts` | 0.3KB | `GET /bridge/adapters` |

后端路由源:`core/management.go` 第 165~205 行,所有路由都在 `/api/v1` 前缀下,挂在同一个 `http.ServeMux`,API 命中后直接返回,**剩余路径都走 SPA 静态文件 + index.html fallback** (`withStaticFallback` 函数,第 215~262 行)。

### CORS 配置

`core/management.go` 第 336-350 行:

```go
func (m *ManagementServer) setCORS(w, r) {
    if len(m.corsOrigins) == 0 { return }  // 默认 nil,不发 CORS 头
    origin := r.Header.Get("Origin")
    for _, o := range m.corsOrigins {
        if o == "*" || o == origin {
            w.Header().Set("Access-Control-Allow-Origin", origin)
            w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
            w.Header().Set("Access-Control-Allow-Headers", "Authorization, Content-Type")
            ...
        }
    }
}
```

默认 **空 corsOrigins → 不发任何 CORS 头**,因此跨 origin 浏览器调 API 会被拦。但 iframe 内同源调用 (page 和 API 都在 `localhost:9820`) 不需要 CORS,**iframe 嵌入场景里这点无关**。

### **关键安全头检查 (决定 iframe 可行性)**

```
gh search code "X-Frame-Options" → 0 hits
gh search code "Content-Security-Policy" → 0 hits
gh search code "frame-ancestors" → 0 hits
```

**cc-connect 服务端没有设置 `X-Frame-Options` / `Content-Security-Policy: frame-ancestors`**,默认 iframe / WebView 嵌入不会被浏览器拦截。Tauri WebView 加载 `http://localhost:9820/login?token=xxx` 会成功显示。

---

## 消息流呈现 (IM 双向)

**结论:cc-connect web 用 WebSocket 长连接 (`/bridge`),不是 SSE,也不是轮询。**

### 长连接拉起流程 (`hooks/useBridgeSocket.ts`)

1. 登录后页面调 `GET /api/v1/status` 拿 `bridge.{port, path, token}` (函数 `fetchBridgeConfig`)
2. **WebSocket URL 不直连 bridge port,而是走当前页面 host + bridge.path**:
   ```ts
   const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
   const wsUrl = `${proto}//${window.location.host}${bridgeCfg.path}?token=${encodeURIComponent(bridgeCfg.token)}`;
   ```
   说明:bridge WS 跟 web 前端**同源同端口** (因为 `withStaticFallback` 检测到 `r.URL.Path == bridgeServer.path` 就转给 bridge handler) → 嵌入场景里也不需要额外开端口
3. WS open → 发 `{ type: 'register', platform: 'web', capabilities: ['text','card','buttons','typing','update_message','preview','reconstruct_reply'], metadata: {...} }`
4. 收 `{ type: 'register_ack', ok: true }` → 状态 `connected`
5. 每 25 秒发 `{ type: 'ping', ts }` 保活
6. 断线 3 秒后自动重连

### 入站消息类型 (`type BridgeIncoming`)

| type | 含义 |
|---|---|
| `register_ack` | 注册响应 |
| `reply` | 一次性完整回复 (text/markdown) |
| `reply_stream` | **流式回复**,带 `delta` + `full_text` + `done` |
| `card` | 富卡片 (header + elements[]),元素含 markdown/divider/note/actions/list_item/select |
| `buttons` | markdown 内容 + 按钮矩阵 |
| `typing_start` / `typing_stop` | 打字指示 |
| `preview_start` | agent 准备开始流式更新 |
| `update_message` / `delete_message` | 增量更新历史 |
| `pong` | 心跳响应 |
| `error` | 协议错误 |

### 出站消息 (用户 → agent)

```ts
{ type: 'message', msg_id: 'web-{Date.now()}', session_key, user_id: 'web-admin',
  user_name: 'Web Admin', content, reply_ctx: sessionKey, project }
```

加 card/button 回调:`{ type: 'card_action', session_key, action, reply_ctx, project }`
加 preview ack:`{ type: 'preview_ack', ref_id, preview_handle }`

### 状态管理

- `useAuthStore` (zustand) — token、isAuthenticated、init/login/logout
- `useThemeStore` (zustand) — light/dark/system
- **聊天消息**:**没用 zustand**,直接组件内 `useState<ChatMsg[]>` (`ChatView.tsx` 第 297 行)
- **没有 react-query / TanStack Query**,所有数据请求都是 `useEffect + fetch` 自己管 loading/error

### Markdown 渲染

- `react-markdown` + `remarkGfm` (表格、删除线、任务列表) + `rehypeHighlight` (代码高亮)
- 自定义 `pre` 块:加复制按钮 + 语言标签
- 自定义 `code` inline:粉色 + 圆角
- `prose` 配 Tailwind typography (`@tailwindcss/typography`)
- card 类型不走 markdown,有专门的 `CardBlock` / `CardElement` 渲染器,处理 list_item / actions / select 等 element 类型

---

## config 编辑器

**结论:不是 Monaco,也没有用 JSON Schema 表单生成器。**

`web/src/pages/System/Config.tsx` (4.1KB) + `GlobalSettings.tsx` (10.5KB):

### Config.tsx (raw config 区)

```ts
const text = await api.raw('/config');   // 拿 raw TOML 或 JSON 字符串
```

- 拿到字符串,检测开头是 `{` / `[` 则 JSON.parse 美化输出,否则当 TOML 原样显示
- **只读!** 渲染成 `<pre className="...whitespace-pre">{content}</pre>`
- 默认折叠,点 ChevronRight 展开
- 顶部有两个按钮:**Reload** (重新读 config.toml,调 `POST /reload`) 和 **Restart** (`POST /restart`)

### GlobalSettings.tsx (结构化表单)

这才是用户实际改设置的地方。**手写的 React 表单**,字段全部 hardcode:

```ts
const LOG_LEVELS = ['debug', 'info', 'warn', 'error'];
const ATTACHMENT_OPTS = ['', 'on', 'off'];
const LANGUAGES = ['en', 'zh', 'zh-TW', 'ja', 'es'];

// 12 个字段全 useState 单独管:
language / attachment_send / log_level / idle_timeout_mins /
thinking_messages / thinking_max_len / tool_messages / tool_max_len /
stream_preview_enabled / stream_preview_interval_ms /
rate_limit_max_messages / rate_limit_window_secs

// 提交:PATCH /settings
await updateGlobalSettings({ language, attachment_send, ... });
```

字段分 5 个 `<Card>` 分组 (General / Display / Stream preview / Rate limit / Log),组件全部用自写的 `Toggle / Select / NumberInput`。

**字段不是从 server 拉 schema,而是写死在前端代码里**;只在加载时 `GET /settings` 拿当前值。Server 端 `GlobalSettings` 类型在 `web/src/api/settings.ts` 定义,跟后端 `core/management.go` 的 `handleGlobalSettings` 手动对齐 (没用 codegen)。

类似的还有 `ProjectDetail.tsx` (35.9KB) 是巨型项目配置表单。

---

## 路由与布局

### 路由 (`web/src/App.tsx`)

```tsx
<Routes>
  <Route path="/login" element={isAuthenticated ? <Navigate to="/"/> : <Login/>} />
  <Route element={<ProtectedRoute><Layout/></ProtectedRoute>}>
    <Route index element={<Dashboard/>} />
    <Route path="projects" element={<ProjectList/>} />
    <Route path="projects/:name" element={<ProjectDetail/>} />
    <Route path="providers" element={<ProviderList/>} />
    <Route path="skills" element={<SkillList/>} />
    <Route path="chat" element={<ChatList/>} />
    <Route path="chat/:name" element={<ChatView/>} />     ← IM 入口
    <Route path="cron" element={<CronList/>} />
    <Route path="system" element={<SystemConfig/>} />
  </Route>
</Routes>
```

- **BrowserRouter** (非 HashRouter) — 注意:Tauri WebView 加载远程 URL 时,刷新会丢路径,因为 Tauri 内部协议跟 BrowserRouter 兼容,但远程 HTTP 资源在 reload 时由 cc-connect 服务端 `withStaticFallback` 处理 SPA fallback (任何非 /api/ 路径都返回 index.html) — **对 iframe 嵌入是友好的**
- **ProtectedRoute** 检查 `useAuthStore.isAuthenticated`,未登录跳 `/login`
- Login 支持 query string `?token=xxx` 自动登录 (`cc-connect web` 命令就是直接打开 `http://localhost:9820/login?token=<urlencode>`)

### 布局 (`web/src/components/Layout/`)

`Layout.tsx`:

```tsx
<div className="flex h-screen overflow-hidden bg-gradient-...">
  <Sidebar />                          {/* 左侧 56px (collapsed 16px) */}
  <div className="flex-1 flex flex-col">
    <Header />                         {/* 顶 14px,刷新/语言/主题/登出 */}
    <main className="flex-1 overflow-y-auto p-6">
      <Outlet />                       {/* 子路由 */}
      <Footer />
    </main>
  </div>
</div>
```

- **侧栏导航 (sidebar)**,**非 tab**。`Sidebar.tsx` 用 `NavLink` 配 7 个固定入口:
  ```
  Dashboard / Projects / Providers / Skills / Chat / Cron / System
  ```
- 侧栏支持 collapsed (16px) / expanded (56px) 切换
- Header 提供:刷新按钮 (派发 `cc:refresh` 自定义事件)、语言切换 dropdown、主题切换、登出
- 整体玻璃态:`bg-white/75 backdrop-blur-xl` + `dark:bg-[rgba(0,0,0,0.85)]`
- 不响应式!`min-w-0` + 固定 sidebar 宽度,小屏会挤;Sidebar 没有自动收缩成 drawer 的逻辑 (移动端体验一般)

---

## iframe 嵌入可行性评估

### **结论:技术上可行,且 cost 比自写一套 UI 低一个数量级。**

### 可行性证据

1. **HTTP server 未阻止嵌入**
   - `gh search code "X-Frame-Options" / "Content-Security-Policy" / "frame-ancestors" → 0 hits` (全 repo 无设置)
   - `withStaticFallback` 处理 SPA fallback 完善,刷新不丢路径
2. **Token 自动登录**:URL `http://localhost:9820/login?token=<urlencode>` 直接登录,无需用户手动填 token (`Login.tsx` 第 30-45 行的 `useEffect` 处理 `?token=` 参数)
3. **Bridge WS 同源**:连 WebSocket 也用 `window.location.host`,在 Tauri WebView 里同源即同地址
4. **没有依赖 IndexedDB / Service Worker / Notification API** (没在 source 里搜到这些,bridge WS + localStorage token 是唯一持久化)
5. **跨平台**:cc-connect 自带 npm/Homebrew/binary 三种安装,可以由 mini-term 用户独立安装,mini-term 只负责"显示面板"

### Tauri WebView 嵌入方式 (两种)

**方案 A:在 mini-term 内开第二个 WebView 窗口**

```rust
// Rust 侧
tauri::WebviewWindowBuilder::new(
    &app,
    "cc-connect-panel",
    tauri::WebviewUrl::External("http://localhost:9820/login?token=...".parse().unwrap())
)
.title("CC-Connect")
.build()?;
```

- 优:**完全无侵入**,cc-connect 整个 UI 直接跑;CSP/同源全部走 cc-connect 服务端策略
- 劣:独立窗口,跟 mini-term 主体看上去不像一体

**方案 B:在 mini-term 主 WebView 内嵌 `<iframe src="http://localhost:9820/login?token=...">`**

- 优:能放进 Allotment 三栏布局里,跟 ProjectList/FileTree 并列;视觉上是一体的
- 劣:需要 mini-term 默认 CSP 允许 `frame-src http://localhost:9820`;tauri.conf.json 当前 CSP 是 `null` (CLAUDE.md 备忘录第 8 项提到这是历史风险),已无 CSP 限制 → iframe 默认能用
- 注意点:
  - **token 不可硬编码在 src URL 的 query 里写到 git**,需要前端动态拼:由 mini-term 通过 Tauri command 读 cc-connect config 拿 token,再渲染 iframe
  - 路由刷新:iframe 内 BrowserRouter 在远程 URL 上工作,refresh 由 cc-connect 服务端 SPA fallback 处理 → 没问题
  - 主题同步:Tauri 主体可能是深色,cc-connect web 主题独立 (`useThemeStore` 存 localStorage `cc_theme`) → 不会自动联动,体验上是两套主题

### iframe 隔离的副作用

- **键盘快捷键不穿透**:mini-term 主进程的全局快捷键 (Ctrl+T 新建终端等) 在 iframe focus 时不响应
- **复制粘贴**:iframe 内独立,从 cc-connect 拷贝出来需要 navigator.clipboard,**两边 origin 不同会触发权限对话框** — Tauri 可以全局放行
- **拖拽 / 文件接力**:iframe 内的拖拽事件不会冒泡到 mini-term 主体
- **关闭面板时长连接清理**:iframe 卸载会自动断 WebSocket,但 cc-connect 后端不会清 session

### 嵌入触发条件

cc-connect 必须先在用户机器上运行!`cc-connect web` 命令只生成 token + 开浏览器,**不启 HTTP server** — server 是 `cc-connect` 主进程开的。所以 mini-term 嵌入面板前需要:

1. 检测 `cc-connect` 是否已启动 (探活 `GET http://localhost:9820/api/v1/status`)
2. 若未启动:提示用户手动启动 `cc-connect` 或 `cc-connect daemon start`
3. 拉 `~/.cc-connect/config.toml` 里的 `management.token` (cc-connect 的 Go 端 `config.EnableWebAdmin` 写入)
4. 拼 `http://localhost:{management.port}/login?token={token}` 嵌入

---

## 我们能复用什么

| 复用项 | 怎么用 | 成本 |
|---|---|---|
| **整套 web UI** | 在 iframe / 第二 WebView 里直接加载 cc-connect 的 `:9820` | **零代码** (只写检测+拼 URL 的 ~50 行胶水) |
| 视觉风格参考 | `Sidebar.tsx` / `Layout.tsx` 的 backdrop-blur + 单 accent 色变量打法 | 我们 Fluent 2 皮肤已经在用类似手法 |
| API 协议 | 如果要自写 mini-term 内置 UI,可以照抄 `/api/v1` REST + `/bridge` WS 的契约 | 中等 (需要 wrap 一份 mini-term 自己的 client) |
| Bridge 协议 | `register / message / card_action / preview_ack / ping/pong` + `register_ack / reply / reply_stream / card / buttons / typing_*` 16 种消息 | 自写需重新实现 |

---

## 我们必须自己写什么

只在选择"自己写 UI 调 API"路线时才需要,如果选 iframe 路线则**全部省掉**:

1. **进程探活 + 启动引导** — 用 Tauri command 探 cc-connect HTTP 是否在,未启动给出引导 / 或者 spawn cc-connect 子进程 (有 instance lock 机制需要绕)
2. **Token 读取** — 解析 `~/.cc-connect/config.toml`,拿 `[management].token` (Rust 端 toml-rs 解析)
3. **(选 iframe 时)只写一个壳** — 一个 React 组件,内部一个 `<iframe>`,props 拿到 token 后拼 src;面板内放 Allotment 子区
4. **(选自写 UI 时)整套 React 重做** — 拷贝 cc-connect 的 12 个 api/*.ts、ChatView (32KB)、ProjectDetail (36KB)、useBridgeSocket;约 **2000-3000 LoC** 直接抄,但需要按 mini-term 现有规范 (zustand store 合并、Tailwind v4 syntax 转换、皮肤变量替换、Allotment 集成) 改约 30%。预估 1-2 周
5. **mini-term 主题与 cc-connect 主题同步** — 两套 useThemeStore 不共享 localStorage,iframe 路线需要 postMessage 桥接;或者把 cc-connect 的 theme 选择 UI 隐藏掉,统一用 mini-term 主题 (会破坏 cc-connect 原版 UX)

### Cost / Value 对比

| 路线 | LoC | 工期 | 维护成本 | UI 一体感 |
|---|---|---|---|---|
| **A. iframe / WebView 嵌入** | ~100 行胶水 (探活 + token 读取 + iframe 壳) | ~1-2 天 | **几乎为零** (cc-connect 升级,我们自动跟随) | 中等 (双套主题,两套快捷键) |
| **B. 自写 UI 调 API** | 2000-3000 行抄改 | 1-2 周首版,持续追 cc-connect 上游 | **高** (cc-connect 每加一个 feature 都要追) | 满分 |

**A 路线 cost/value 显著更优** — 尤其考虑 cc-connect 是 10K+ stars 活跃项目,接口和 UI 仍在持续变动 (这次 v1.3.0 刚加 Skills + Global Provider Management)。

---

## 引用

- 仓库: <https://github.com/chenhg5/cc-connect>
- 默认分支 main,pushed_at 2026-05-28T02:07:57Z
- 关键文件 (raw 链接):
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/package.json>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/vite.config.ts>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/embed.go>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/main.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/App.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/api/client.ts>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/hooks/useBridgeSocket.ts>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/pages/Chat/ChatView.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/pages/System/Config.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/pages/System/GlobalSettings.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/components/Layout/Layout.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/components/Layout/Sidebar.tsx>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/web/src/store/auth.ts>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/cmd/cc-connect/web.go>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/core/management.go>
  - <https://raw.githubusercontent.com/chenhg5/cc-connect/main/core/web_assets.go>
- README 关于 `cc-connect web` 的描述: README.md "🆕 What's New in v1.3.0" 章节
- gh code search 验证 (执行 2026-05-28):
  - `X-Frame-Options` / `Content-Security-Policy` / `frame-ancestors` 全 0 hits
  - `Access-Control-Allow-*` 只出现在 `core/bridge.go` 和 `core/management.go` 的 setCORS 函数 (需要显式 corsOrigins 才启用)

## Caveats / Not Found

- **未实测**:本研究全靠静态代码阅读,没有真的跑 `cc-connect web` + Tauri WebView 端到端验证 iframe 是否真能加载、键盘焦点是否符合预期、WebView 跨进程 localStorage 隔离行为
- **CSP 来源**:虽然 cc-connect 服务端没设 CSP,**mini-term 自己的 tauri.conf.json** 是否设了 CSP 会影响 iframe 加载;CLAUDE.md 内存条目"2026-04-11 全量 review 发现"提到"CSP=null",所以当前 mini-term 主体没设 CSP → iframe 默认能用,但是这是一个**遗留安全弱点**,如果将来收紧 CSP 需要把 `http://localhost:9820` 加 `frame-src`/`connect-src` 白名单
- **未抓全的页面**:`Providers/ProviderList.tsx`、`Skills/SkillList.tsx`、`Cron/CronList.tsx`、`ProjectDetail.tsx`、`SessionChat.tsx` 等大文件没逐字看完,但通过 `api/*.ts` 已经掌握全部 API 接触面,UI 实现细节对本次"是否能嵌入"决策影响不大
- **bridge 端口与 management 端口的关系**:`vite.config.ts` dev 代理写的是 `/bridge → :9810`,但 production 路径下 bridge 跟 management 是**同一个 server 同一个端口** (9820),因为 `withStaticFallback` 检测到路径匹配 `bridgeServer.path` 直接 hijack 走 WS handler;这意味着嵌入时只需要暴露/连接一个端口
- **未确认 v1.3.0 → 后续版本兼容性**:研究基于 main 分支当前 commit,如果用户机器装的是更老的 cc-connect,API 路径可能不完全一致 (例如 `/skills` 是 v1.3.0 新加的)

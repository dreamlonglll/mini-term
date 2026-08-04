# 「使用统计」功能设计（Usage Stats）

> 参考来源：`tmp/usage-stats-migration.md`（cc-sessions-viewer，主要蓝本）、
> `tmp/usage-stats-migration-overview.md`（cc-switch，仅借鉴口径）。
> 状态：设计稿，未实施。

## 1. 方案选型

两份参考文档代表两条路线：

| | 文档 1（cc-switch） | 文档 2（cc-sessions-viewer） |
|---|---|---|
| 存储 | SQLite 明细表 + 日汇总表 + Rollup/Prune | 无库，每次扫描 + 内存聚合 |
| 采集 | 内置代理实时记账 + 会话日志导入 + 跨源去重 | 仅本地 JSONL 转录文件 |
| 定价 | 定价表 CRUD + models.dev 同步 + 零成本回填 | models.dev 拉取 + 24h 缓存 + 兜底锚点 |
| 前端 | TanStack Query + 轮询/事件/手动三合一刷新 | 流式事件 + 骨架替换渲染 |

**采用文档 2 的架构**。理由：

- mini-term 没有数据库、没有代理层。文档 1 的核心复杂度（代理记账、跨源去重、rollup 控库体积）全部建立在「应用本身是 API 代理」的前提上，对 mini-term 是负资产。
- mini-term 的 `ai_sessions.rs` 已经在扫同一批数据源（`~/.claude/projects/*/*.jsonl`、`~/.codex/sessions/**/rollout-*.jsonl`），文件发现、cwd 编码匹配、路径归一化逻辑可直接复用。
- Tauri 事件通道（`useTauriEvent` hook）、Modal 外壳、i18n 命名空间、Zustand 均现成，文档 2 的「后台线程 + 代际取消 + 节流推送」模式与本项目 `search.rs`（start/cancel + 事件流）完全同构。

文档 1 仅借鉴统计口径（缓存命中率公式、筛选级联清空规则）；若未来 mini-term 引入代理/记账能力再回头参考其存储设计。

## 2. v1 功能范围

### 2.1 控制维度

- **scope**：`all`（整机全部会话）/ `project:<path>`（单项目，路径与项目列表的登记项目对应）。agent 筛选（claude / codex）作为独立开关叠加。
- **range**：`today` / `days7` / `days30` / `month` / `months3` / `months6` / `custom:起:止`（上限 1 年）。**不提供 all**（全盘扫描太重）。
- scope / range / agent 选择持久化到 localStorage（同 `themeManager.ts` 模式）。

### 2.2 展示区块（自上而下）

| 区块 | 数据 | 备注 |
|---|---|---|
| KPI ×4 | 总成本(USD)、API 调用数、会话数、缓存命中率 | 命中率 = cacheRead ÷ (input + cacheRead + cacheCreation) |
| Token 副行 | 输入 / 输出 / 缓存读 / 缓存写 | K/M/B 缩写 |
| 每日活动图 | 每天 cost（线）+ calls（柱） | 纯 SVG/CSS 实现，**不引图表库**；仅 1 天数据退化为摘要卡 |
| 按项目排行 | 前 8 项目：成本横条 + 会话数 | 展示名取 JSONL 内 `cwd` 真实路径（目录名编码有损，不可反解）；已登记项目可点击切入单项目 scope |
| Top Sessions | 前 10 最贵会话 | 点击 → 复用现有 `SessionViewerModal` 查看正文 |
| 按模型 | 前 6 + Others | 横条即可，短名展示（"Opus 4.7"）由通用规则推导，不维护映射表 |
| 常用工具 / Shell 命令 / MCP | 各前 10 计数排行 | Shell 首词提取按文档 2 §6.9 |
| 进度指示 | processed/total + partial 流式充实 | |

### 2.3 明确砍掉（v1 不做）

| 项 | 理由 |
|---|---|
| 13 类活动分类器 | 400+ 行主观规则，价值/成本比最低，留待后续迭代 |
| 账号额度徽标（OAuth usage API） | 未公开接口 + 钥匙串读取 + 429 敏感，与终端管理器定位关系弱 |
| 托盘快速统计 | `feature/tray-status-light` 正在改托盘，避免搅合；后续可作扩展 |
| WSL / SSH 远程会话统计 | 9P / SFTP 全量读代价过高；v1 只统计本机 host 会话 |
| SQLite 落库 / rollup | 见 §1 选型 |

## 3. 数据来源与解析

### 3.1 文件发现（复用 + 扩展 `ai_sessions.rs`）

- `all` scope：枚举 `~/.claude/projects/` 下**全部**项目目录 + `~/.codex/sessions/` 全部 rollout（现有代码是按项目 cwd 过滤，统计需要全局枚举，新增入口函数，复用 `collect_codex_session_paths` 等原语）。
- `project:` scope：直接复用 `find_claude_project_dirs` / Codex cwd 匹配。
- **Claude 子代理转录必须纳入**（`<projectDir>/<sessionId>/subagents/*.jsonl`，实现时核实真实布局）——独立计费，漏掉会整块低估成本。
- mtime 粗筛：`session.mtime < range.lo` 直接跳过（仅省解析，不是过滤判定）。

### 3.2 逐行解析 → Turn / CallRecord

从 assistant 行提取：`message.model`、`message.id`（去重键）、`message.usage.{input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, cache_creation.ephemeral_5m/1h_input_tokens}`、`content[].tool_use`（name + input）、`timestamp`。

与聊天正文渲染（`get_ai_session_content`）是**两条独立解析路径**，统计侧轻量单遍、不构造消息体。

⚠️ **实现时验证点**：Codex rollout 的 usage 载体（`event_msg/token_count` 为累计口径，需差分或取末值）文档 2 未给细节，必须对照真实文件确认后再写解析。

## 4. 后端设计

新模块目录 `src-tauri/src/usage_stats/`：

| 文件 | 职责 |
|---|---|
| `mod.rs` | Tauri commands + 流程编排 + 代际取消 |
| `turns.rs` | JSONL → `Vec<Turn>`（文件内同 message_id 合并） |
| `aggregate.rs` | Aggregator：跨文件去重、窗口终判、各维度累加、快照排序 |
| `pricing.rs` | 接收前端传入的价格表，归一化查价 + 成本公式 |

命令与事件（对齐 `search.rs` 的 start/cancel 模式）：

```
start_usage_stats(request_id, scope, range, agents, pricing) → 立即返回，工作进 std::thread
cancel_usage_stats()                                          → 代际计数 +1

事件：usage-stats-progress { requestId, processed, total, partial }
     usage-stats-done     { requestId, stats }
     usage-stats-error    { requestId, error }
```

- 取消 = 全局 `AtomicU64` 代际：新请求 store、取消 fetch_add；worker 每处理一个文件比对，不等即静默退出。前端同样丢弃 `requestId` 不匹配的事件（双保险）。
- 节流推送：每 16 个文件或 250ms（先到为准）。
- 聚合结果 `AgentStats` 结构照搬文档 2 §4.3，serde camelCase，`src/types.ts` 手写镜像。

## 5. 定价层

- **前端拉价**：`src/utils/modelPricing.ts` 用浏览器 `fetch('https://models.dev/api.json')`（与 `updateChecker.ts` 同模式，免去 Rust 侧新增 HTTP 依赖及 rustls/ring 约束核对），归一成 `{ model: {input, output, cacheRead, cacheWrite} }`（$/token，÷1e6）后经 invoke 传给后端。
- localStorage 缓存，**24h TTL**；失败时用过期缓存兜底。
- **无硬编码价格表**。拉取失败且无缓存 → UI 渲染错误占位 + Retry，**不显示全 0 成本假数据**（文档 2 教训）。
- 后端查价：原名 → 剥 `@pin`/日期后缀 → canonical（去 provider 前缀、点转横线）→ 最长前缀匹配 → 三锚点均价兜底（文档 2 §6.6）。

## 6. 正确性口径（迁移时必须原样保留，均来自文档 2 §6 的真实 bug）

1. **message_id 双层去重**：文件内同 id 多行合并（usage 取 total 大的一侧）；跨文件全局 HashSet 命中整条跳过；无 id（Codex）不参与；全部被去重的会话不计入 session_count。
2. **两层时间过滤**：mtime 只是粗筛；**逐 turn 按自身 timestamp 终判**（缺失回退 mtime）。
3. **本地日历日**：today = 本地 00:00 起（绝不用滚动 24h，历史上放大过 7 倍）；month = 月初；days7/30 = 含今天的完整日历日。日活分桶**统一本地时区**（修正文档 2 自认的 UTC 小坑）。
4. **成本公式**：`input×Pin + output×Pout + reasoning×Pout + cacheWrite×Pcw + cacheWrite1h×Pcw×0.6 + cacheRead×Pcr`；cache_creation 兼容 legacy 整数与 split 两种形状，total 取 max(legacy, 5m+1h)，1h 子集钳到 ≤ total。
5. **排序截断**：cost 降序；全 $0 时退化为按 tokens/calls 降序；by_tool 等 count 降序 + name 升序 tie-break。
6. **价格未就绪不渲染 KPI**；单日数据不画孤点图。

## 7. 前端设计

- **入口**：`ActivityBar` 新增柱状图图标（设置/SSH/移动端一组），打开 `UsageStatsModal`；`App.tsx` 加 `statsOpen` state（同 `mobileOpen` 模式）。抽屉仅 340px 宽，不适合仪表盘，故用宽 Modal（`w-[960px] max-h-[85vh]`，复用 `Modal.tsx`）。
- **组件**：`src/components/usage/` — `UsageStatsModal.tsx`（筛选 + 状态机）、`KpiCards`、`DailyChart`（纯 SVG）、`RankBarList`（横条排行通用件，项目/模型/工具/Shell 复用）、`TopSessions`。
- **数据 hook**：`src/hooks/useUsageStats.ts` — 订阅三事件、管理 requestId、卸载时 unlisten + cancel（对应文档 2 的 useStatsStream；关 Modal 即停扫描）。
- **状态优先级**（互斥渲染）：价格失败(Retry) ＞ 价格加载中 ＞ 扫描错误 ＞ 骨架(无 partial) ＞ 空态 ＞ 主体。
- **i18n**：新 `locales/usageStats.ts` + `locales/index.ts` 注册，zh/en 双语。
- 金额统一两位小数，`<$0.01` 单独显示。

## 8. 涉及的现有文件（改动面）

| 文件 | 改动 |
|---|---|
| `src-tauri/src/lib.rs` | `mod usage_stats;` + 注册 2 个 command |
| `src-tauri/src/ai_sessions.rs` | 个别 `fn` 提升 `pub(crate)`（全局枚举复用），不改行为 |
| `src/App.tsx` | `statsOpen` state + 渲染 Modal |
| `src/components/ActivityBar.tsx` | 新图标按钮 + prop |
| `src/types.ts` | `AgentStats` 等类型 |
| `src/i18n/locales/index.ts` | 注册命名空间 |

其余全部为新增文件。实现前对上述现有符号跑 GitNexus impact 分析（项目规约）。

## 9. 实施阶段与验证

| 阶段 | 内容 | 验证 |
|---|---|---|
| P1 | `turns.rs` + `aggregate.rs` + `pricing.rs` 纯函数 + 单测 | `cargo test`：去重不翻倍 / 全重复会话不计数 / 逐 turn 窗口过滤（顶层 KPI == 窗内 daily 之和）/ today=本地午夜 / 成本公式含 1h 档 / 前缀匹配不误塌 |
| P2 | 流式命令 + 事件 + 代际取消 | 手动：切 scope/range 不串数据 |
| P3 | 拉价 util + hook + Modal UI + i18n | `npm run build` + `npm run tauri dev` 走查各状态 |
| P4 | 对账 | 与 `npx ccusage`（同数据源社区工具）抽样比对总成本与 Today；Today 视图是历史 bug 高发区，重点核对 |

建议在当前 `feature/tray-status-light` 合并后从 main 拉新分支实施（改动面无交集，但避免混提交）。

## 10. 开放问题

1. Codex usage 载体格式（见 §3.2 ⚠️），P1 首个任务就是拿真实 rollout 文件定格式。
2. `all` scope 首扫整机可能数千文件：mtime 粗筛 + range 上限已缓解，若实测仍慢，可加「最近 N 天文件数」预估并提示。
3. opencode 会话（文档 2 支持）：mini-term 的 AI 识别里有 opencode 命令，但 `ai_sessions.rs` 未扫其转录；v1 先 claude/codex，opencode 待有需求再加。

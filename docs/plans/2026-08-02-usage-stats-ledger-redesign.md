# 使用统计账本化重构 —— 统一规划

> 日期：2026-08-02 ｜ 分支：`feature/usage-stats-smooth`（决策 2A：在此分支上继续重构）
> 存储选型：rusqlite（决策 1A）｜ 图表选型：recharts v3（用户指定，弃手绘 SVG）
> 执行方式：新会话按本文档实施。实施前先读本文档全文与「现状与待删清单」。

## 0. 背景与目标

现状问题：每次打开/刷新使用统计都从原始 JSONL 全量（或增量解析缓存）扫描聚合，
展示与采集耦合——打开有等待感、刷新有状态机复杂度、手绘 SVG 动效生硬。

目标（对齐 cc-switch 体验）：
1. **采集与展示彻底分离**：原始 JSONL 只在采集时读一次 → 落中间账本（SQLite）；
   展示永远查账本，毫秒级秒出，任何参数切换都是纯查询，无扫描态。
2. 老数据一次性 backfill，新数据增量追加（文件监听 + 打开面板触发）。
3. 图表换 recharts（自带进入/更新补间、Tooltip），删除手绘 SVG 与手写 rAF 补间。

## 1. 账本设计（rusqlite，存 `{app_data_dir}/usage.db`）

依赖：`cd src-tauri && cargo add rusqlite -F bundled`（bundled 静态编译 SQLite，
无系统依赖；实施时以 cargo 拉到的最新版为准，API 以 Context7 `/rusqlite` 核对）。
打开时设 `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;`。

```sql
CREATE TABLE IF NOT EXISTS sessions (
  session_id  TEXT PRIMARY KEY,
  agent       TEXT NOT NULL,            -- 'claude' | 'codex'
  cwd         TEXT,
  title       TEXT,
  provider    TEXT,
  file_path   TEXT NOT NULL,            -- 主转录/rollout 路径(最后一次解析来源)
  mtime_ms    INTEGER NOT NULL          -- 该文件粗筛 mtime(aggregate 的 ts 回退用)
);

CREATE TABLE IF NOT EXISTS turns (
  request_id     TEXT PRIMARY KEY,      -- 见 §1.1 身份规则
  session_id     TEXT NOT NULL,
  ts_ms          INTEGER,               -- 可空(缺时间戳的 turn,查询时回退 session.mtime_ms)
  model          TEXT,
  input          INTEGER NOT NULL DEFAULT 0,
  output         INTEGER NOT NULL DEFAULT 0,
  reasoning      INTEGER NOT NULL DEFAULT 0,
  cache_read     INTEGER NOT NULL DEFAULT 0,
  cache_write    INTEGER NOT NULL DEFAULT 0,
  cache_write_1h INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts_ms);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);

CREATE TABLE IF NOT EXISTS sync_state (
  file_path TEXT PRIMARY KEY,
  mtime_ms  INTEGER NOT NULL,
  size      INTEGER NOT NULL
);
```

**成本不落库**：查询时按前端传入的定价表现算（定价会更新，落库会算死；
turns 表规模小，内存聚合毫秒级）。

### 1.1 turn 身份规则（幂等主键，决定去重语义）

- Claude：有 `message_id` → `claude:{message_id}`（全局唯一，fork 复制历史跨
  session 同 id → 主键天然去重，upsert 归属最后解析的会话，cwd 相同无影响）；
  无 id → `noid:{session_id}:{顺序号}`。
- Codex：`codex:{session_id}:{顺序号}`。顺序号 = parse_codex_session 返回的
  turns 向量下标（append-only 文件下稳定）。**血缘前缀跳过（早于 session_meta
  的复制历史）与相邻重复守卫已在 turns.rs 实现（4fd6683 之前的提交），原样保留
  ——fork 复制历史在解析层就不产出 turn，账本无需再管 Codex 跨会话去重。**
- 写入用 UPSERT（`ON CONFLICT(request_id) DO UPDATE`，last-write-wins）：
  Claude 流式快照后期变大、文件重解析都能收敛到最新值。

### 1.2 同步策略（增量追加，文件粒度）

不做行级 offset 续读（Codex 差分基线跨批次维护复杂、易错）。
**文件粒度重解析**：`sync_state` 记 (path, mtime, size)；
- 未变 → 跳过（绝大多数文件，一次 stat 的成本）；
- 变了/新文件 → 整文件重解析（单文件解析本来就快）→ 全部 turn UPSERT →
  更新 sync_state。重写/compact/回卷天然被主键幂等吸收。
- Claude job 的 mtime 取主转录与 subagents/*.jsonl（含 workflows/wf_*/ 深层，
  见 §5 采集完整性）的最大值——任一子文件更新触发整组重解析。
- 文件从磁盘删除：账本保留（历史仍然花过钱）；sync_state 残留无害。

触发时机：
1. **backfill**：账本首次创建（或 schema 版本升级）时全量同步一次，后台线程，
   emit 进度事件；
2. 打开统计面板时触发一次同步；
3. 面板开着时：复用 fs.rs 的 notify 基建监听 `~/.claude/projects` 与
   `~/.codex/sessions`（递归），有写入事件即同步该文件（去抖 ~1s）；
   若接 notify 改动面大，可退化为面板内 5s 定时同步（同步本身极廉价）。

并发控制：全局 `Mutex` 保证同一时刻只有一个同步在跑；Connection 每次
命令内打开（简单可靠），或全局 OnceLock<Mutex<Connection>> 二选一。

## 2. 后端命令与事件（替换旧扫描协议）

删除：`start_usage_stats` / `cancel_usage_stats` 命令、
`usage-stats-progress` / `usage-stats-done` / `usage-stats-error` 事件、
GENERATION 代际、PARSE_CACHE 进程内解析缓存（4fd6683 引入，被账本替代）。
lib.rs 注册表同步更新。

新增（usage_stats/ledger.rs）：
- `usage_ledger_query(params) -> UsageStatsPayload`（同步命令，快）：
  按窗口/agent/项目 scope 查 turns+sessions → 组回 `ParsedSession` 结构 →
  **喂现有 Aggregator**（daily/hourly/DST 分桶/rankings/topSessions 全复用，
  UsageStatsPayload 序列化形状不变 → 前端 types.ts 零改动）。
  项目 scope 过滤沿用 normalize + 子路径规则（mod.rs 现有 in_scope 逻辑迁入）。
- `usage_ledger_sync()`：触发一次增量同步（内部去重并发），完成后 emit
  `usage-ledger-synced {added: usize}`；backfill 期间另 emit
  `usage-ledger-progress {processed, total}`。

解析复用：turns.rs 的 parse_claude_session / parse_codex_session 原样复用；
枚举文件复用 mod.rs 的 collect_claude_jobs / collect_codex_jobs（去掉 since_ms
mtime 粗筛——账本要全量历史，窗口过滤移到查询层）。

## 3. 前端重构

### 3.1 useUsageStats.ts 重写（状态机大幅简化）

```
打开面板 → loadModelPricing → invoke usage_ledger_query(秒出) → 渲染
         → invoke usage_ledger_sync(后台)
'usage-ledger-synced' → 重新 query（数据更新，recharts 自动补间）
'usage-ledger-progress' → 仅 backfill 首次显示进度文案
切参数 → 直接重新 query（毫秒级，无需快照缓存/静默机制）
自动刷新 → 定时 sync（synced 事件驱动重查）
```

删除：statsCache 参数级快照缓存、silentRef 静默机制、
partial/processed/total 流式状态（保留 backfill 进度）、
phase 里的 scanning 态（简化为 pricing/pricingError/ready/error）。
骨架屏保留但只在「backfill 未完成且查询结果为空」时出现。

### 3.2 DailyChart 换 recharts（v3，实施时 `npm i recharts`）

props 不变（`{daily, range}`，fillBuckets 补零逻辑保留），内部替换为：

```tsx
<ResponsiveContainer width="100%" height={232}>
  <ComposedChart data={buckets}>
    <CartesianGrid strokeDasharray="3 4" stroke="var(--border-default)" />
    <XAxis dataKey="date" tickFormatter={...MM-DD/HH:00} fontSize={9} />
    <YAxis yAxisId="cost" orientation="left" tickFormatter={axisCost} />
    <YAxis yAxisId="calls" orientation="right" tickFormatter={formatCount} />
    <Tooltip content={<UsageTooltip />} />   {/* 自定义,复用现有 tooltip 行样式 */}
    <Bar yAxisId="calls" dataKey="calls" fill="var(--text-muted)" opacity={0.28} radius={[2,2,0,0]} />
    <Area yAxisId="cost" type="monotone" dataKey="cost" stroke="var(--accent)" fill="url(#usage-daily-area)" strokeWidth={1.8} />
  </ComposedChart>
</ResponsiveContainer>
```

**v3 注意事项（Context7 已核对）**：
- 自定义 tooltip 的类型是 `TooltipContentProps`（v2 的 `TooltipProps` 已改名），
  `label` 可能为 `undefined | string | number`；
- 多轴时 `Tooltip` 有 `axisId` prop 指定跟随哪根轴；
- `CartesianGrid` 新增 `x/yAxisId`——多 YAxis 下 grid 需显式指定
  `yAxisId="cost"`，否则可能不渲染；
- 多 YAxis 按 `yAxisId` 字母序渲染（不是 JSX 顺序）；
- SVG 无 z-index，层叠按 JSX 渲染顺序，Tooltip 放最后。
- 主题色全部走现有 CSS 变量；渐变 defs 保留现有 `usage-daily-area`。

删除：ChartSvg 手绘实现、useTween.ts 里图表用的 useTweenedNumbers
（KPI 数字滚动 useTweenedNumber 保留——recharts 不管 KPI）。
RankBarList 宽度过渡、入场动画、骨架屏组件、Section shadow 保留。

## 4. 现状与待删清单（分支 feature/usage-stats-smooth @ 4fd6683）

保留：turns.rs 全部（血缘去重）、aggregate.rs 全部（查询层复用）、
pricing.rs、KPI 数字滚动、骨架屏、RankBarList 过渡、usage-fade-in、
fillBuckets、i18n 词条（scanning 相关词条改为 backfill 进度词条）。

删除/替换：mod.rs 的 PARSE_CACHE + GENERATION + run_scan + start/cancel 命令、
useUsageStats 的 statsCache/silentRef/partial 流、DailyChart 的 ChartSvg 手绘、
useTween.ts 的 useTweenedNumbers（数组版）。

## 5. 采集完整性顺带修正（调研 cc-switch 时发现，实施时一并处理）

1. Claude 子代理目录要覆盖 `subagents/workflows/wf_*/*.jsonl` 深层
   （检查 collect_claude_jobs 现状，漏了会系统性少算 Workflow 用量）。
2. 计费门槛确认为「任一计费维度 > 0 即计入」（cc-switch 实测旧式
   stop_reason+output>0 双重过滤系统性低估 ~4%，检查 turns.rs 现状）。

## 6. 实施顺序（新会话按此推进，每步可验证）

1. `cargo add rusqlite -F bundled`，建 ledger.rs：schema + open + upsert +
   sync_file/sync_all + 单元测试（tempdir 假 JSONL → 落库 → 幂等重跑数量不变）。
2. query：rows → ParsedSession → Aggregator，单测对齐现有 aggregate 测试口径
   （同一批假数据走「旧 run_scan 内存路径」与「落库再查」结果一致）。
3. 新命令 + lib.rs 注册 + 删旧命令/事件/缓存；`cargo test` 全绿。
4. 前端 useUsageStats 重写 + Modal 状态机简化；`npm run build`。
5. `npm i recharts`（v3.3+），DailyChart 重写 + 自定义 Tooltip;删手绘/数组补间。
6. notify 监听（或降级定时同步）+ backfill 进度 UI。
7. 手工验收：首次打开(backfill 进度) → 秒开 → 切参数秒切 → AI 会话进行中
   图表自动长出新数据（recharts 补间）→ 重启后秒开（账本持久化）。

## 7. 风险与回退

- rusqlite bundled 编译时间 +~1min，一次性；若目标机 cargo 网络受限需先
  `cargo fetch`。
- recharts 包体积 ~100KB gz：懒加载 Modal 已存在（启动优化批次），随弹窗
  chunk 加载，不影响启动。
- 账本损坏：打开失败即删除重建 + 触发 backfill（数据源头是 JSONL，账本可再生，
  无需备份机制）。
- 数据口径变化风险：§6.2 的新旧路径一致性测试兜底。

---

## 附注（2026-08-04 实施更新）

- 账本 schema 已升 **v3**：turns 主键改为 `(session_id, request_id)` 并增 `message_id`
  列（fork 归属确定化、Claude 收缩收敛，跨文件去重回聚合层首见规则）；新增
  `tool_events` 表承载工具/Shell/MCP 排行（原设计 §2.2 遗留项落地）。
- 上文「UsageStatsPayload 序列化形状不变」自 v3 起不再成立：payload 追加
  `byTool` / `byShell` / `byMcp`（各前 10 计数，`UsageCountStat {name, count}`），
  前端 types.ts 同步扩展。版本不匹配即删表重建，由空 sync_state 触发 backfill。
- **展示层不渲染这三个排行**（2026-08-04 手长决定：仅采集不展示；token 消耗本就
  完整——Claude 工具块的 token 计入所在 assistant 消息 usage，Codex 由 token_count
  事件覆盖）。命令改 async（同步命令跑主线程,busy_timeout 等锁会冻住整窗）；版本
  重建放 IMMEDIATE 事务内原子化；backfill 期间前端按进度事件节流(1s)重查实现增量填充。

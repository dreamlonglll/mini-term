# 批次规格：用量面板补全（#17）+ 会话面板补全（#18）

> 对应 `docs/gpui-parity-audit.md` 第 2 层缺口 #17（第 47 行）与 #18（第 48 行）。
> 基线：Tauri 前端 `src/`（权威对照）；落点：`crates/mt-app/src/usage_panel.rs`（1124 行）
> 与 `crates/mt-app/src/session_panel.rs`（754 行）。
>
> 本文只描述**要做什么、按什么口径做**，不写实现代码。每条规格标源文件与行号。
> 行号取自本文撰写时的工作副本（`gpui` 分支 `e91bb03`），改动后可能漂移，以符号名为准。

---

## 0. 共用前置（两节都要先读）

### 0.1 已就绪的基建（别重复造）

| 能力 | 落点 | 说明 |
|------|------|------|
| 账本查询 | `mt_usage::usage_ledger_query`（`crates/mt-usage/src/ledger.rs:500-532`）| 已有 `until_ms: Option<i64>` 形参 —— **custom range 的上界不需要改 mt-usage** |
| 增量同步 | `spawn_usage_ledger_sync` / `SyncEvent{Progress,Synced}`（`ledger.rs`）| 已接，见 `usage_panel.rs:441-473` |
| 价格表类型 | `mt_usage::ModelPrice`（`crates/mt-usage/src/pricing.rs:10-19`）| 字段 `input/output/cache_read/cache_write`，单位 **$/token** |
| 兜底均价 | `PricingTable`（`pricing.rs:22-28`）| 查价全失败时取三锚点均价；表为空才真记 $0 |
| 分支边扫描 | `mt_ai::sessions::scan_session_lineage`（`crates/mt-ai/src/sessions.rs:1793-1803`）| **已实现且已导出**（`mt-ai/src/lib.rs:60` 导出 `LineageEdge`）；mt-app 零引用 |
| 自记账边存储 | `AppConfig::session_lineage: Vec<SavedLineageEdge>`（`crates/mt-config/src/config.rs:200`、结构 `207-213`）| 已在磁盘格式里，mt-app 零消费 |
| 会话视图偏好 | `AppConfig::session_list_view: Option<String>`（`config.rs:194`）| 已在磁盘格式里，mt-app 零消费 |
| 上下文菜单 | `crates/mt-app/src/menu.rs`（`show` / `item` / `separator` / `MenuItem::danger`，`menu.rs:135-236`）| N 批落地，可直接用 |
| 命令式弹窗 | `crates/mt-app/src/prompt.rs`（`open_guarded:47` / `show_alert:213` / `Confirm:144`）| N 批落地 |
| 品牌图标 | `mt_ui::icons::{AiVendor, BrandIcon}`；`AiVendor::for_session`（`crates/mt-ui/src/icons/brand.rs:120-124`）| 模型优先、回落 CLI |
| 状态灯 | `mt_ui::icons::{StatusDot, StatusKind, SPIN_PERIOD}`（`crates/mt-ui/src/icons/status.rs:71,216,229-261`）| ai-working 自带 900ms 真旋转 |
| 矢量弧线 | `mt_ui::icons::vector::Geom::Arc`（`crates/mt-ui/src/icons/vector.rs:89`、`arc_points:165`）| 自绘 spinner 用 |
| 配色/控件 | `crates/mt-app/src/ui.rs`（`bg_*` / `text_*` / `accent` / `border_*` / `color_info:287` / `ghost_button:297` / `section_title:357`）| |
| 配置写盘 | `AppStore::save_config_soon`（`crates/mt-app/src/store.rs:1460`，500ms 去抖）| 偏好持久化走这条 |

### 0.2 ⚠️ gpui-component 组件不可直接用于本批

`gpui-component 0.5.1` **随包 0 个 svg 文件**（`find . -name "*.svg" | wc -l` → 0），
`IconName::path()` 返回 `"icons/xxx.svg"` 交给宿主 `AssetSource` 解析。mt-app 没注册
AssetSource → **一切走 `IconName` 的组件都渲染空白，且编译期无感**（N 批已在菜单上踩过，
见 `docs/gpui-parity-audit.md:32`）。本批受影响的具体组件：

- `date_picker`（`src/time/date_picker.rs:426` 用 `IconName::Calendar`）+ `calendar`
  （`src/time/calendar.rs:631,710` 用 `ArrowLeft/ArrowRight` 翻月）→ **日历翻不了月，不可用**
- `select`（`src/select.rs:856` 用 `ChevronDown`，`:298` 用 `Inbox`）→ 下拉箭头空白
- `spinner`（`src/spinner.rs:24` 默认 `IconName::Loader`）→ 转的是个空框
- `skeleton`（`src/skeleton.rs:41-47`）没有图标，**但取色走 `cx.theme().skeleton`**，
  与壳 `Palette` 不同源（J 批同一条硬伤）→ 只能借它的动画思路，不能直接用

结论：本批的日期输入、下拉、spinner、骨架屏**一律自绘**，与 K/M/N 批同一路线。

---

## 1. #17 用量面板补全

落点 `crates/mt-app/src/usage_panel.rs`。原版 `src/components/usage/UsageStatsModal.tsx`（536 行）
+ 五个子件 `KpiCards.tsx` / `DailyChart.tsx` / `RankBarList.tsx` / `TopSessions.tsx` / `format.ts`
+ 两个纯逻辑模块 `src/utils/usageDates.ts` / `src/utils/modelPricing.ts`
+ 数据流 hook `src/hooks/useUsageStats.ts`。

### 1.1 差异清单

| # | 能力 | 原版 | GPUI 现状 | 判定 |
|---|------|------|-----------|------|
| A | agent scope 四档分段 | `UsageStatsModal.tsx:44,409` | `usage_panel.rs:303-342,603-624` | ✅ |
| B | range 分段 | `usageDates.ts:10-12`（**七档，含 custom**）| `usage_panel.rs:46-98`（**六档，无 custom**）| 🟡 |
| C | custom 自选起止日期 | `UsageStatsModal.tsx:161-162,208-221,424-444` | 无（`until_ms` 恒传 `None`，`usage_panel.rs:413`）| ❌ |
| D | 自动刷新档位 0/5/10/30/60s | `UsageStatsModal.tsx:23,163-166,179-183,447-458` | 无 | ❌ |
| E | 单项目下拉 | `UsageStatsModal.tsx:150-159,193-201,411-422` | 「当前项目」二值开关（`toggle_project_scope:491-497`）| ❌ |
| F | 排行条点击切 scope | `UsageStatsModal.tsx:346`、`RankBarList.tsx:12,32` | `rank_row` 无 `on_click`（`usage_panel.rs:539-582`）| ❌ |
| G | Top 会话点开查看 | `TopSessions.tsx:8,27`、`UsageStatsModal.tsx:379,385-393,493-498` | 只读列表（`usage_panel.rs:922-960`）| ❌ |
| H | 骨架屏 | `UsageStatsModal.tsx:111-137,285-286` | 「加载中…」纯文本（`usage_panel.rs:747-757`，注释里已认账）| ❌ |
| I | KPI 数字滚动 | `useTween.ts:8-27`、`KpiCards.tsx:72-76` | 无补间 | ❌ |
| J | 偏好持久化 | localStorage 六个键（`UsageStatsModal.tsx:15-20`）| 全内存；跨重启丢 | ❌ |
| K | 价格表拉取 | `modelPricing.ts:179-206`（fetch models.dev + 24h 缓存）| 只读本地 `model-pricing.json`（`usage_panel.rs:292-298`）| ❌ |
| L | 四相位互斥渲染 | `useUsageStats.ts:15-16`、`UsageStatsModal.tsx:263-289` | 「价格空 → 挂黄边提示 + 照常出 KPI」（`usage_panel.rs:718-737`）| ❌ |
| M | 手动刷新 = 先等同步跑完再查 | `useUsageStats.ts:145-169`（`wait: true`）| `refresh():499-503` 先起同步再查（= 旧版被修掉的实现，数字会跳）| ❌ |
| N | 刷新按钮 syncing 忙态置灰 | `UsageStatsModal.tsx:461-469` | 无 | ❌ |
| O | 错误态 / 空态 / Retry 按钮 | `StateHint`（`UsageStatsModal.tsx:503-536`）| 错误只是列一行红字（`usage_panel.rs:738-745`）| ❌ |
| P | KPI 五格 | `KpiCards.tsx:78-110`（cost / tokens / calls / sessions / cacheHit）| 四格，sessions 并进 calls 副标题（`usage_panel.rs:765-806`）| 🟡 |
| Q | Token 副行 in/out/cached/written | `UsageStatsModal.tsx:304-321` | 无 | ❌ |
| R | 趋势图（补空桶 + 双轴 + hover 详情 + 单桶摘要卡）| `DailyChart.tsx:26-71,75-108,174-185,186-253` | 等宽柱 + 首尾日期（`usage_panel.rs:809-851`）| 🟡 |
| S | 模型 Top6 + Others 合并 | `UsageStatsModal.tsx:24,233-261` | `take(8)` 无 Others（`usage_panel.rs:877`）| 🟡 |
| T | 三卡同行 + 项目卡固定高滚动 | `UsageStatsModal.tsx:329-376`（`max-h-[216px]`，`:333`）| 纵向堆叠、不限高 | ❌ |
| U | Section 卡片壳（竖条标题 + 边框 + 背景）| `UsageStatsModal.tsx:99-109` | 只有 `ui::section_title` 一行标题 | 🟡 |
| V | byTool / byShell / byMcp 三段 | **原版没有** | `usage_panel.rs:962-987` | GPUI 多出，**保留** |
| W | 数值格式化五函数 | `format.ts:3-62` | `usage_panel.rs:143-267` 逐条对齐 + 单测 `1006-1047` | ✅ |
| X | 本地日历日窗口起点 | `usageDates.ts:38-69` | `range_since_ms:105-131` + 单测 `1073-1108` | ✅ |
| Y | 查询丢后台 + 竞态序号 | `useUsageStats.ts:76,90` | `usage_panel.rs:390-438` | ✅ |

审计条目点名的 8 条（C/D/E/F/G/H+I/J/K）写在 §1.2；顺带补齐的（L/M/N/O/P/Q/R/S/T/U）
写在 §1.3，规格从简但同样必做——它们和点名项落在同一片代码上，分批做会互相返工。

### 1.2 点名缺口的实现规格

#### C. custom 自选起止日期

**枚举扩容**：`UsageRange` 加 `Custom` 变体（`usage_panel.rs:47-54`），
`ALL` 数组长度 6→7，`key()` 返回 `"custom"`（与 TS 联合类型字面量一字不差，
见 `usageDates.ts:11`），`label()` 取 `t("usageStats","range.custom")`（词条已在，
`src/i18n/locales/usageStats.ts:15/73`），`hourly()` 对 `Custom` 返回 `false`。

**日期边界语义**（逐条照抄 `src/utils/usageDates.ts`，这文件是纯函数、可直接移植成 Rust 并配单测）：

| 语义 | 源 | Rust 侧规格 |
|------|-----|-------------|
| 一年下限 | `customFloor`（`usageDates.ts:32-34`）| `today - 364 天` 的本地日历日 |
| 起点 | `rangeSinceMs`（`:62-69`）| `custom` 时：`from` 解析失败/缺失 → **回落 days30 起点**（不是报错、不是无下界）；解析成功 → `max(from, floor)` |
| 终点 | `rangeUntilMs`（`:72-90`）| 非 custom 恒 `None`（开区间到现在）；custom：`to` 解析失败 → `None`；`from > to`（键盘可造出的倒置区间）→ **把上界抬到 `from`**，等效单日查询，不许静默全零；`day < floor` → 抬到 `floor`；最终返回 `该日 +1 天的 00:00 - 1ms`（含截止日全天） |
| 图表补桶窗口 | `customChartWindow`（`:95-106`）| 与查询窗口**同源**：起点走 since、终点走 until 的日历日；`end < start` 时 `end = start`。轴反映**所选窗口**而非数据跨度 |
| 提交闸门 | `acceptDateInput`（`:115-117`）| `^\d{4}-\d{2}-\d{2}$` 才更新承诺值，其余输入保持上一个有效值（受控输入随之回弹）。**空串一旦进查询，custom 会静默退化成无上界/近 30 天，展示远大于所选范围的数据** |

**查询接线**：`UsagePanel::query`（`:390-438`）目前第 4 个实参写死 `None`
（`ledger.rs:504` 的 `until_ms`），改为按 range 计算；`since` 同理走新的 custom 分支。

**控件**：两个自绘文本输入（`YYYY-MM-DD`，`gpui-component` 的 `input::TextInput` 无图标依赖可用，
或直接自绘），仅在 `range == Custom` 时渲染（对齐 `UsageStatsModal.tsx:424`）。
`min`/`max` 在原版只是 `<input type="date">` 的 `:invalid` 提示、**不拦截键入**（`usageDates.ts:31` 注释明说），
所以 Rust 侧不必模拟，钳位由上表的 since/until 规则兜住。
失焦/回车时过闸门，通过才写状态 + 落盘（见 §1.2 J）。

**测试**（对齐既有单测风格，中文函数名）：一年下限钳位、倒置区间退单日、
空串回弹、custom 图表窗口与查询窗口同源。

#### D. 自动刷新档位

**档位**：`[0, 5, 10, 30, 60]` 秒，`0 = 关`（`UsageStatsModal.tsx:23`）。**默认 5s**
（`:164` 的 `loadPref(..., '5')`；设计要求「面板开着时至少 5s 定时同步」）。

**语义**：定时器触发的是**只同步不阻塞**的那一路 —— 原版 `sync()`（`useUsageStats.ts:202-204`）
= `invoke('usage_ledger_sync', { wait: false })`，**不直接重查**；数据有变由
`usage-ledger-synced` 事件驱动补查（`:190-198`，且 `added == 0` 时跳过重查避免无谓重渲染）。
GPUI 侧对应：调 `spawn_usage_ledger_sync`（`usage_panel.rs:441-473` 的 `start_sync` 已实现这条链路，
`SyncEvent::Synced{added}` → `added > 0` 才 `query`）。**定时器不得直接调 `query`**。

**计时器生命周期**（这条是本项的核心风险，原版靠 React effect 天然管住，GPUI 要自己管）：
- 只在**面板可见**时跑。`UsagePanel` 实体在首次打开后**常驻不销毁**（`main.rs:441-448`：
  `usage_panel` 一旦 `Some` 就不再置回 `None`，只靠 `usage_open` 过滤渲染，`main.rs:692`），
  所以**必须像 `SessionPanel::set_visible` 那样显式接一个可见性开关**
  （范式见 `session_panel.rs:174-182`），在 `AppRoot::toggle_usage`（`main.rs:441-448`）里调用。
  否则关掉面板后定时器还在每 5s 扫会话文件。
- 存成 `_refresh_task: Option<Task<()>>`，赋新值即 drop 旧的（与 `_query_task` / `_sync_task`
  同一模式，`usage_panel.rs:361,363` 已有注释说明「一份而不是一列表」）。
- 档位改为 0 → 直接把 `_refresh_task` 置 `None`。
- 循环体：`cx.background_executor().timer(Duration::from_secs(n)).await` 后回主线程触发同步，
  再进下一轮；`this.update` 返回 `Err`（实体已释放）即退出循环。
- 档位切换要**重建**任务而不是等当前 sleep 走完（60s→5s 时用户要立刻看到效果）。

**控件**：自绘下拉，位置紧挨手动刷新按钮（原版 `:447-458`，注释「语义自明」）。
`0` 显示 `t("usageStats","autoRefreshOff")`，其余显示 `"{n}s"`（**不进字典**，原版 `:455` 就是裸模板串）。
下拉的 tooltip 用 `t("usageStats","autoRefresh")`。

#### E. 单项目下拉

**数据来源**：`AppStore::projects()`（`store.rs:189`）→ `&[ProjectConfig]`，
**顺序照原样**（原版 `:417` 直接 `projects.map`，不排序 —— 与项目列表同序，用户找得到）。
选项值是 `ProjectConfig.path`（**不是 id**），因为 `usage_ledger_query` 的 `project_path`
形参走 cwd 匹配（`ledger.rs:522-525` 的 `session_in_scope`）。

**语义**（原版 `:150-159`）：
- 与 agent scope **独立叠加**（设计合同 §2），不是互斥
- 持久化存 **raw 路径**；项目已被移除时**回落整机**而不是空结果 ——
  `effectiveProject = projectPath && projects.some(p => p.path === projectPath) ? projectPath : null`
  （`:158-159`）。GPUI 侧同样要在 render 时按当前项目表做这道有效性过滤，
  **不能在读盘时一次性判定**（项目可能在面板开着的时候被删）
- 首项是 `t("usageStats","scope.allProjects")`（值为空 = 整机）

**替换点**：删掉现有的 `toggle_project_scope`（`usage_panel.rs:491-497`）与它在 header 里的
按钮（`:658-674`）。现有实现有两个偏差：只能选「当前活动项目」，且标签会随活动项目漂移。

#### F. 排行条点击切 scope

**只有「按项目」这一栏可点**（`RankBarList.tsx:12` 的 `onClick` 是可选项，
只有 `UsageStatsModal.tsx:346` 传了）。规则（`:294-297,341-347`）：

- cwd ↔ 登记项目路径的匹配走归一：`p.replace(/\//g,'\\').toLowerCase().replace(/\\+$/,'')`
  （`:295`；注释说明「对齐后端 normalize」）。Rust 侧同规则。
- **只有匹配到已登记项目的行才可点**；未登记项目（跑过 AI 但没加进 mini-term 的目录）
  仅展示、无 hover 态、无指针（`:346` 的 `registered ? ... : undefined`）
- 点击动作 = 切入单项目 scope（等价于把 E 的下拉选到该项目），**并落盘偏好**
- 可点行的样式：`cursor-pointer -mx-1.5 px-1.5 rounded-sm hover:bg-[var(--border-subtle)]`
  （`RankBarList.tsx:28`）。注意原版在滚动容器上配了 `-mx-1.5 px-1.5` 吸收这个出血
  （`UsageStatsModal.tsx:332-333` 有整段注释：不吸收的话滚动容器的 `overflow-x` 被强制成 auto，
  行超宽会恒出横向滚动条）—— GPUI 的滚动容器行为不同，但**内外边距要同源**这条照抄

#### G. Top 会话点开查看

**点开后展示什么**：复用会话正文查看器。原版是把 `UsageTopSessionStat` 转成 `AiSession`
再喂给 `SessionViewerModal`（`UsageStatsModal.tsx:385-393,493-498`）：

```
id           ← viewer.sessionId
sessionType  ← agent === 'codex' ? 'codex' : agent === 'grok' ? 'grok' : 'claude'   // :388-389
title        ← viewer.title
timestamp    ← viewer.timestamp
projectPath  ← viewer.projectPath        // 单独作为 prop 传，:497
```

**数据从哪来**：`mt_usage::TopSessionStat`（`crates/mt-usage/src/aggregate.rs:34-45`）
已含 `session_id / agent / project_path / project_name / title / timestamp / cost / calls / tokens`
—— 转换所需字段**一个不缺**。正文本身走 `mt_ai::sessions::get_ai_session_content(session_type, session_id, project_path, wsl_distro)`
（签名见 `session_panel.rs:300-306` 的调用点），`wsl_distro` 传 `None`
（账本只收本机来源；Top 会话没有 WSL/远程标识）。

**GPUI 落法**：`SessionPanel` 已有一个面板内正文预览（`session_panel.rs:281-433`），
但它是 `SessionPanel` 的私有状态、不可复用。两条路二选一：
1. **推荐**：在 `UsagePanel` 里加一份同构的 `Preview`（结构照抄 `session_panel.rs:111-118`），
   点击 Top 会话行时切到预览视图、带「‹ 返回」按钮（照抄 `:399-408`）。
   代价是两份近似代码 —— 但把预览抽成公共件属于**另一条缝**，本批不动。
2. 抽 `mt-app` 内部的 `session_preview` 模块给两边共用（更干净，但会改到 `session_panel.rs`
   的既有渲染路径，与 §2 的改动撞车，需在同批内排序）。

无论哪条，正文加载**雷打不动丢 background executor**（`session_panel.rs:296-307` 已有注释：
正文可能几 MB）。加载中/失败态复用 `t("sessionViewer","loading")` 与红字（`:336-353`）。

**行样式**（`TopSessions.tsx:23-54`）：整行是按钮，
`日期(76px 等宽) | 项目名(150px 截断) | 标题(flex-1 截断) | 横条(110px) | 成本(min 56px 右对齐) | 调用数(min 48px 右对齐)`，
`hover:bg-[var(--border-subtle)]/60`。标题空串时显示 `t("usageStats","untitled")`（`:37`）。
横条基准：cost 榜首；**全 $0（价格缺失）时退化按 tokens**（`:18-19`）——
GPUI 侧 `bar_ratios_or`（`usage_panel.rs:280-286`）已是同一语义，直接用。

#### H+I. 骨架屏 / 数字滚动动画

**骨架屏触发条件**（`UsageStatsModal.tsx:285-286`）：
`!stats || (stats.sessionCount === 0 && backfillTotal > 0)` —— 即「查询还没回」**或**
「backfill 正在跑且账本还空」。注意它在相位机里的位置：排在 pricing/error 之后、空态之前（见 §1.3 L）。

**骨架形状**（`BodySkeleton`，`:120-137`，注释说明目的是「避免转圈 → 完整布局跳变」）：
```
grid 4 列 × h-66px            ← KPI 行（原版 5 格但骨架画 4 格，照抄）
h-4  w-320px                  ← Token 副行
h-280px                       ← 趋势图
flex 3 等分 × h-200px         ← 三卡同行
```
每块：`rounded-[var(--radius-md)]`(6px) + `bg-[var(--border-subtle)]` + 脉冲动画。

**动画实现**：自绘。Tailwind `animate-pulse` 是 2s 的 `opacity: 1 → .5 → 1`；
gpui 侧用 `.with_animation(id, Animation::new(2s).repeat().with_easing(bounce(ease_in_out)), |el, d| el.opacity(1.0 - d*0.5))`
（思路照 `gpui-component/src/skeleton.rs:47-56`，**但取色用 `ui::border_subtle()` 不用 `cx.theme().skeleton`**）。
⚠️ `with_animation` 会**持续请求帧**（K 批在滚动条淡出上专门绕开过它）——
骨架屏是短命状态可以接受，但**必须在 `stats` 到位后立刻停止渲染骨架**，
不能留在树上靠 `opacity(0)` 藏起来。

**数字滚动**（`useTween.ts:3-27`）：
- 时长 `400ms`，缓动 `easeOutCubic`：`1 - (1-t)^3`（`:5`）
- 从**上一次显示值**补到新目标（`currentRef`，`:10,18`），动画中途目标又变时从当前显示值继续
- `from === target` 直接返回、不起动画（`:13`）
- 五个 KPI 各自独立补间（`KpiCards.tsx:72-76`）；`cacheHit` 为 `null` 时显示 `—` **不补间**（`:108`）
- 显示时 tokens/calls/sessions 取 `Math.round`（`:90,95,101`），cacheHit 取 `toFixed(1)`（`:108`）
- GPUI 落法：面板持一份 `tween: HashMap<&'static str, (from, to, start_instant)>` 或五个具名字段，
  在 `query` 回填 stats 时记录起点，render 时按 `Instant::now()` 求值。
  ⚠️ **不要每格挂一个 `with_animation`**（五个独立动画各自请求帧）；
  用一个「有任一 tween 未结束就 `cx.notify()`」的单点重绘更省

#### J. 偏好持久化

原版存 **localStorage** 六个键（`UsageStatsModal.tsx:15-20`）：

| 键 | 类型 | 默认 | 校验 |
|----|------|------|------|
| `mini-term-usage-scope` | `all\|claude\|codex\|grok` | `all` | 白名单（`loadPref:26-34`） |
| `mini-term-usage-range` | 七档字面量 | `days30` | 白名单；**存量的 `'all'` 不在白名单，自动回落 days30**（`:45-47` 注释） |
| `mini-term-usage-project` | raw 路径 或 缺失 | 缺失 = 整机 | 不校验值，渲染时按项目表过滤（`:158-159`） |
| `mini-term-usage-autorefresh` | `0\|5\|10\|30\|60` | `5` | 白名单 + `Number.isFinite`（`:163-166`） |
| `mini-term-usage-custom-from` | `YYYY-MM-DD` | 29 天前 | 正则（`loadDatePref:57-65`） |
| `mini-term-usage-custom-to` | `YYYY-MM-DD` | 今天 | 正则 |

**GPUI 侧存哪**：没有 localStorage。**存进 `AppConfig`**（`crates/mt-config/src/config.rs`），
理由与 `session_list_view`（`:194`）、`locale`（L 批）同源。字段规格：

```rust
/// 用量面板偏好。全部 Option/宽松类型 —— 手改坏值不许拖垮整份 config
/// 连带丢掉项目列表（与 locale 存 String 不存枚举同一条理由，见 L 批）。
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage_scope: Option<String>,          // "all"|"claude"|"codex"|"grok"
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage_range: Option<String>,          // "today"|…|"custom"
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage_project: Option<String>,        // 项目绝对路径；None = 整机
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage_auto_refresh: Option<u32>,      // 秒；0 = 关
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage_custom_from: Option<String>,    // "YYYY-MM-DD"
#[serde(default, skip_serializing_if = "Option::is_none")]
pub usage_custom_to: Option<String>,
```
- `skip_serializing_if` 保旧格式互读（新版写的 config 老版能读、反之亦然）
- `Default::default()` 里补 `None`（对齐 `config.rs:500-501` 的写法）
- 读取一律过白名单/正则，不合法回默认（**不写回、不报错**，与 `loadPref` 同）
- 写入走 `AppStore::save_config_soon`（`store.rs:1460`，500ms 去抖）——
  连点分段控件不会连写六次盘
- store 需新增 setter，命名跟随既有范式（`set_theme_mode:1203` / `set_terminal_font_size:1264` /
  `set_right_drawer_width:1337`）：建议单个 `set_usage_prefs(prefs, cx)` 一把写，
  避免六个 setter 各自触发一次去抖

⚠️ **`UsagePanel::new` 目前不读任何偏好**（`usage_panel.rs:367-387` 硬编码
`Scope::All` / `UsageRange::Days30` / `project_scope: None`）；且它在首次打开时才构造
（`main.rs:441-448`），构造时 config 一定已加载完，直接读即可。

#### K. 价格表拉取 ⭐（本批最大的一块）

##### K.1 原版做了什么（`src/utils/modelPricing.ts`，206 行）

| 项 | 值 | 行号 |
|----|-----|------|
| URL | `https://models.dev/api.json` | `:19` |
| 请求头 | **无自定义头**，裸 `fetch(PRICING_URL)`（浏览器自带 UA/Accept）| `:187` |
| 失败判定 | `!resp.ok` → `throw new Error('HTTP ' + status)` | `:188` |
| 缓存位置 | localStorage `mini-term-model-pricing` | `:20` |
| 缓存 TTL | 24h | `:21` |
| 缓存版本 | `CACHE_VERSION = 2`；**建键规则变更时 +1**，版本不符不当新鲜值 | `:23,182` |
| 缓存结构 | `{ version, fetchedAt, table }` | `:32-37,192-195` |
| 空表判定 | 归一后 0 条 → `throw new Error('empty pricing table')` | `:190` |
| 失败降级 | 有旧缓存（含过期/旧版本）→ **用旧缓存**；无缓存 → 抛错 | `:200-205` |
| 上层降级 | 已有内存价格表时拉新失败**静默沿用**，不把可用面板打成错误态 | `useUsageStats.ts:113-127` |

**响应结构**（models.dev api.json 是 provider → models 两层）：
```jsonc
{
  "<providerId>": {
    "models": {
      "<modelId>": {
        "cost": { "input": <number, $/1M>, "output": <number>,
                  "cache_read"?: <number>, "cache_write"?: <number> }
      }
    }
  }
}
```

**归一规则**（`normalizePricingTable:111-165`，这是全文最容易做错的一段）：
1. `cost.input` / `cost.output` 任一不是 number → 跳过该模型（`:124`）
2. `input === 0 && output === 0` → **丢弃**（订阅制 provider 的占位价；
   收下会把该模型整段成本抹成 0，**比查不到价更糟**——查不到还有兜底均价，`:125-127`）
3. 建键按 **canonical** 形式（`canonicalModelKey:47-54`）：
   `trim → 小写 → 取最后一个 '/' 之后 → 剥 '@' 之后 → '.' 换 '-'`。
   顺序要紧：**先取 `/` 后段再剥 `@`**。
   ⚠️ 与 `mt_usage` 侧的同名归一必须**逐规则对齐**（原版注释指向
   `src-tauri/src/usage_stats/pricing.rs::canonical()`，GPUI 侧对应
   `crates/mt-usage/src/pricing.rs:30` 起的那段）。不对齐的话前端择优出来的键
   在后端二次塌陷，择优白做。
4. 同键碰撞用**全序**比较器择优（`comparePricingCandidates:76-84`），依次比：
   ① 一方 provider（`anthropic`/`openai`/`xai`）优先 → ② modelId 含一方前缀次优先
   （`:28-30,64-68`）→ ③ 有显式 `cache_read` → ④ 有显式 `cache_write` →
   ⑤ providerId 字典序**小者胜** → ⑥ modelId 字典序小者胜。
   **最后两级是为了「不平局」**：择优结果只由候选集合决定、与遍历顺序无关（`:70-75` 注释）。
   遍历本身也按键排序（`sortedEntries:99-103`）。
   不做这件事的症状是「面板每刷新一次总额就换一个值」（`:12-16` 注释原话）。
5. 单位换算：四个值全部 `÷ 1e6`（`:147-150`），得到 **$/token**，
   正好是 `mt_usage::ModelPrice` 的口径（`pricing.rs:10-19`）
6. 碰撞数量在控制台留痕（`:156-163`）——GPUI 侧用 `log`/`eprintln` 等价即可

##### K.2 GPUI 侧要什么 HTTP 能力

需求很窄：**一次 HTTPS GET，无自定义头，读整个 body 成字符串，解析 JSON**。
不需要重定向以外的复杂特性、不需要流式、不需要 multipart、不需要 cookie。
可以完全跑在 `cx.background_executor()` 上（**绝不能落主线程**：DNS + TLS 握手动辄几百 ms）。

##### K.3 工作区已有的 HTTP / TLS 依赖盘点（`Cargo.lock` 实测）

**直接声明的**（`crates/*/Cargo.toml`）：
| crate | 依赖 | 位置 |
|-------|------|------|
| mt-relay | `tokio-tungstenite = "0.27"`（features `rustls-tls-native-roots`）| `crates/mt-relay/Cargo.toml` |
| mt-relay | `rustls = "0.23"`（`default-features = false`，features `ring, std, tls12, logging`）| 同上 |
| mt-relay | `tokio`（workspace：`rt-multi-thread, sync, time, io-util, macros`）| 工作区 `Cargo.toml` |
| mt-app | `futures = "0.3"`、`serde_json`、`chrono`、`iana-time-zone` | `crates/mt-app/Cargo.toml` |

**已在依赖树里（传递依赖，编译一次不花第二份时间）**：
| 包 | 版本 | Cargo.lock 行 | 来源 |
|----|------|---------------|------|
| `zed-reqwest` | `0.12.15-zed` | `8394` | `gpui_http_client 0.2.2`（`:2503`）← `gpui`（`:2359`）|
| `hyper` | `1.11.0` | `2857` | 同上 |
| `hyper-rustls` | `0.27.9` | `2878` | 同上 |
| `hyper-util` | — | `2894` | 同上 |
| `http` / `http-body` / `http-body-util` | 1.x | `2803/2813/2823` | 同上 |
| `rustls` | `0.23` | `5499` | mt-relay + zed-reqwest 共用 |
| `rustls-native-certs` | — | `5514` | 同上 |
| `tokio-rustls` | — | `6706` | 同上 |
| `url` | `2.5.8` | `7120` | gpui_http_client |
| `tokio` | 1.x | — | mt-relay + zed-reqwest |

**不在树里**：`reqwest`（上游原版）、`ureq`、`isahc`、`curl`、`attohttpc`（grep 计数 0）。

**gpui 自带的 HTTP 抽象**：
- `gpui` 重导出 `http_client`（`gpui/src/gpui.rs:82`）
- `App::http_client() -> Arc<dyn HttpClient>`（`gpui/src/app.rs:1166`）
- `Application::with_http_client(...)`（`gpui/src/app.rs:165`）
- ⚠️ **默认实现是 `NullHttpClient`，`send()` 直接 `bail!("No HttpClient available")`**
  （`gpui/src/app.rs:2344-2369`）。`gpui_http_client` crate 本身**只提供 trait 与 `AsyncBody`，
  不提供 reqwest 后端实现**（Zed 把那部分放在未发布的 `reqwest_client` crate 里）——
  `cx.http_client()` 拿到手就是个恒失败的桩，**不能直接用**。

##### K.4 推荐方案

**方案 A（推荐，净新增 crate = 0）**：mt-app 直接依赖已在树里的 `zed-reqwest`，打开 `blocking`：
```toml
# crates/mt-app/Cargo.toml
reqwest = { package = "zed-reqwest", version = "0.12.15-zed", default-features = false,
           features = ["blocking", "rustls-tls-native-roots", "charset", "http2"] }
```
- `blocking` feature 只额外要 `futures-channel` + `tokio/sync`（`zed-reqwest/Cargo.toml:79-85`），
  两者都已在树里
- reqwest 自带 `tokio = { features = ["net","time"] }`（`:497-503`），
  blocking 客户端内部起 `new_current_thread().enable_all()` 运行时（`src/blocking/client.rs:1198-1199`），
  **不需要 mt-app 自己接 tokio**
- 阻塞调用丢 `cx.background_executor().spawn(...)`，与 `usage_ledger_query` 同一模式
  （`usage_panel.rs:405-421`）
- ⚠️ **rustls CryptoProvider**：rustls 0.23 需要进程级默认 provider。mt-relay 已经
  `default-features = false` + `ring`，reqwest 走 `__rustls-ring`（`zed-reqwest/Cargo.toml:69-74`），
  两边同为 ring → 默认 provider 无歧义。但如果 mt-app 在 mt-relay 接线之前先用上 HTTP，
  **要确认 `rustls::crypto::ring::default_provider().install_default()` 在首次请求前被调过一次**
  （mt-relay 的注释里写了「运行时需 install ring provider」）

**方案 B（备选，依赖树更干净但净新增 ~8 个 crate）**：`ureq 3`（同步、无 tokio、可复用树里的 rustls 0.23）。
如果不希望 mt-app 直接碰 reqwest 的巨大 feature 面，走这条。

**方案 C（不推荐）**：实现 `gpui::http_client::HttpClient` 并 `with_http_client` 注入。
对本批（一个 GET）是过度设计；但如果后续 #30 的「版本检查/更新提醒」也要拉网，
届时可以把 A 包成一个 `HttpClient` 实现统一注入。

##### K.5 缓存与降级（GPUI 版）

- **缓存落盘位置**：`{app_data_dir}/model-pricing.json`——**沿用现在这个文件**
  （`usage_panel.rs:292-298` 的 `load_local_pricing` 已经读它）。
  但要改成带信封的格式，与原版 localStorage 结构同构：
  ```jsonc
  { "version": 2, "fetchedAt": <epoch ms>, "table": { "<canonicalKey>": {"input":…,"output":…,"cacheRead":…,"cacheWrite":…} } }
  ```
  ⚠️ **兼容裸表**：现有实现读的是**裸 `HashMap<String, ModelPrice>`**（用户可能已经按
  `pricingLocalHint` 的指引手放了一份，见 `usageStats.ts:51/109` 的文案）。
  反序列化要先试信封、再试裸表，裸表当作「无 TTL、永不过期的手工表」处理并**优先于网络**
  ——不能把用户手放的表在第一次成功拉网后悄悄覆盖掉。建议：手工裸表存在时，
  网络结果写进另一个文件名（如 `model-pricing.cache.json`），手工表恒优先。
- **TTL**：24h，与 `CACHE_TTL_MS` 同（`modelPricing.ts:21`）
- **版本**：`CACHE_VERSION = 2`，不符不当新鲜值但仍可离线兜底（`:181-183,200-204`）
- **降级链**（照 `:200-205` + `useUsageStats.ts:113-127`）：
  新鲜缓存 → 拉网成功 → 过期/旧版缓存 → 已有内存表（静默沿用）→ 报 `pricingError`
- **刷新时机**：面板打开时 + 手动刷新时各走一次 `ensure_pricing`（TTL 命中即瞬时）。
  原版特意不把「内存里有表」当作永久绕过 TTL 的理由（`:109-111` 注释：
  应用常驻多日后过期价格照常重拉）——GPUI 侧同样是常驻进程，**这条必须照抄**

##### K.6 相位与 UI（与 §1.3 L 合并实现）

拿不到价格时**绝不渲染 KPI**（全 0 会误导，`UsageStatsModal.tsx:264-265` 原话）。
现有实现（`usage_panel.rs:718-737`）是「挂一条黄边提示 + 照常出全 0 数据」，**违反这条红线，必须改**。
`pricingLocalHint` 词条（M 批补的，`usageStats.ts:51/109`）在有网络拉取后仍保留价值
（离线/内网环境的补救办法），放进 `pricingError` 态的 detail 行。

### 1.3 顺带补齐（审计条目未点名，但同片代码）

- **L 四相位互斥**：`pricingError > pricing(且无旧数据) > error > 骨架 > 空态 > 主体`
  （`UsageStatsModal.tsx:263-289`）。GPUI 侧建一个 `Phase` 枚举，render 顶部单点分派。
- **M 手动刷新语义**：改成「先 `usage_ledger_sync(wait=true)` 跑完，再 query」
  （`useUsageStats.ts:145-169`，注释明说旧实现「每点一次必然先闪一次同步前的旧值」）。
  mt-usage 侧 `usage_ledger_sync`（`mt-usage/src/lib.rs:33` 导出）是就地阻塞版，
  丢 background executor 后 `await` 即可。
- **N 忙态**：`syncing` 期间刷新按钮置灰 + 不可点（`:461-469`）。
- **O 状态提示件**：等价 `StateHint`（`:503-536`）：可选 spinner + 主文案 + detail（截断 480px，
  hover 出全文）+ 可选动作按钮。Retry 动作 = `refresh`。
- **P KPI 五格**：`cost(accent 色值) / tokens / calls / sessions / cacheHit`（`KpiCards.tsx:78-110`）。
  tokens = `input+output+cacheRead+cacheWrite`（`:70`，与排行榜 tokens 同口径）。
  现有实现把 sessions 塞进 calls 的副标题（`usage_panel.rs:777-782`），改回独立一格。
  图标五枚（wallet/stack/pulse/chat/bolt）是 16 viewBox stroke path（`KpiCards.tsx:7-34`），
  用 mt-ui 的 `VectorIcon` DSL 照点移植（与 M 批边条同一手法）。
- **Q Token 副行**：`in | out | cached | written`，分隔符是 `--border-strong` 色的 `|`
  （`UsageStatsModal.tsx:304-321`）。
- **R 趋势图**：优先级从高到低——① **补空桶**（`fillBuckets:26-71`：today 从 00:00 到当前小时、
  日粒度补到今天、custom 补所选窗口；后端快照是稀疏的，不补就画不出完整时间轴）；
  ② 单桶退化成摘要卡（`:174-185`，孤点图没有信息量）；③ hover 详情六行
  （`UsageTooltip:75-108`：总 Token/in/out/cached/费用/调用数，各带色点）；④ 双轴。
  gpui-component 有 `chart` / `plot` 模块可评估，但注意它同样走自己的 theme token。
- **S 模型 Top6 + Others**：`TOP_MODELS = 6`（`:24`），剩余合并成一行
  `t("usageStats","othersModels", {count})`，**Others 也参与 max 归一**（`:257`）；
  全 $0 时按 tokens 排比例（`:234`）。
- **T 三卡同行**：byProject | byModel | byProvider 等分横排，项目卡 `max-h-216px` 内滚动（`:329-376`）。
- **U Section 卡片壳**：`border-[--border-subtle] rounded-md bg-[--bg-elevated]/40 px-4 py-3.5 shadow-sm`
  + 标题前 `w-0.5 h-3.5 rounded-full bg-[--color-info]` 竖条（`:99-109`）。

### 1.4 i18n key 清单（#17）

命名空间 `usageStats`，字典源 `src/i18n/locales/usageStats.ts`（zh 行 / en 行），
Rust 侧已由 `gen_from_ts.mjs` 生成进 `crates/mt-i18n/src/dict.rs:1866` 起的段落。
**下列 key 全部已存在双语，本批无需往 TS 源头补词条。**

| key | zh 行 | 用在哪 |
|-----|-------|--------|
| `title` | 3 | 面板标题（已用） |
| `scope.all` | 5 | scope 分段（已用） |
| `scope.allProjects` | 6 | **E 单项目下拉首项** |
| `range.today/days7/days30/month/months3/months6` | 9-14 | range 分段（已用） |
| `range.custom` | 15 | **C custom 档位** |
| `kpi.cost` / `kpi.calls` / `kpi.cacheHit` | 18,20,22 | KPI（已用） |
| `kpi.tokens` | 19 | **P 第二格** |
| `kpi.sessions` | 21 | P 独立成格（现被当副标题用） |
| `tokens.in/out/cached` | 25-27 | 已用 |
| `tokens.written` | 28 | **Q Token 副行第四项** |
| `dailyActivity` | 30 | 已用 |
| `byProject` / `byModel` / `byProvider` | 31-33 | 已用 |
| `byTool` / `byShell` | 34-35 | 已用（M 批补的） |
| `topSessions` | 36 | 已用 |
| `refresh` | 37 | 已用 |
| `autoRefresh` | 38 | **D 下拉 tooltip** |
| `autoRefreshOff` | 39 | **D 的 0 档** |
| `unknownModel` | 40 | 已用 |
| `unknownProvider` | 41 | **供应商空串时**（现直接显示空串，`usage_panel.rs:912`）|
| `othersModels`（含 `{count}`）| 42 | **S Others 行** |
| `tip.totalTokens` / `tip.cost` | 44-45 | **R hover 详情** |
| `progress`（`{processed}` `{total}`）| 47 | 已用 |
| `backfilling` | 48 | 已用 |
| `pricingLoading` | 49 | **K/L 拉价中** |
| `pricingError` | 50 | 已用（语义要改回「拉取失败」）|
| `pricingLocalHint` | 51 | 已用（并进 pricingError 的 detail）|
| `scanError` | 52 | **O 查询失败** |
| `retry` | 53 | **O Retry 按钮** |
| `empty` | 54 | **O 空态** |
| `noDailyData` | 55 | **R 无数据** |
| `noSessions` | 56 | **F/G 排行与 Top 会话空态** |
| `untitled` | 57 | **G 标题空串** |
| `callsCount`（`{count}`）| 58 | **R 单桶摘要卡** |

不进字典的裸字面量（照原版）：`"Claude"` / `"Codex"` / `"Grok"`（厂商名，`UsageStatsModal.tsx:223-227`）、
`"MCP"`（`usage_panel.rs:971`）、`"5s"/"10s"/…`（`:455`）、`"–"` 日期分隔符（`:434`）。

### 1.5 样式要点（#17，取自 `src/styles.css`）

| 变量/类 | 值 | 行 |
|---------|-----|----|
| `--radius-sm` | `4px`（blueprint/fluent2 皮肤下 `2px`）| `62` / `1002,1051` |
| `--radius-md` | `6px`（皮肤下 `3px`）| `63` / `1003,1052` |
| `--border-subtle` | `rgba(255,255,255,0.05)` | `26` |
| `--border-default` | `rgba(255,255,255,0.08)` | `27` |
| `--border-strong` | `rgba(255,255,255,0.12)` | `28` |
| `--color-info` | `#6896c8` | `39` |
| `--color-ai` | `#b08cd4` | `37` |
| `--color-success` / `--color-warning` / `--color-error` | `#6bb87a` / `#d4a84a` / `#d4605a` | `34-36` |
| `--shadow-overlay` | `0 8px 32px rgba(0,0,0,.5), 0 0 1px rgba(255,255,255,.05)` | `45` |
| `.usage-fade-in` | `usageFadeIn 0.35s ease-out`；`opacity 0→1` + `translateY(6px)→none`；**刻意不加 forwards** | `242-249` |
| `.usage-rank-bar` | 宽度 `transition-[width] duration-500 ease-out`（在 `RankBarList.tsx:39`）| `475-477` |
| reduced-motion 豁免 | 用量面板的入场与排行条补间**照常播**（`471-477`，理由写在 `465-470`）| — |

排行条几何（`RankBarList.tsx:37-53`）：轨道 `w-14`(56px) `h-1.5`(6px) `rounded-full`
`bg-[--border-subtle]`；填充 `linear-gradient(90deg, var(--color-info), var(--color-ai))`，
宽度 `max(min(ratio,1)*100, 2)%`（**最小 2% 保底**，全 0 时也看得见槽位）；
主值 `min-w-14 text-[13px] font-medium`，次值 `min-w-10 text-xs text-muted`。

⚠️ **`ui::Palette` 缺 `color_ai`（`#b08cd4`）与 `border_strong`**
（`crates/mt-app/src/ui.rs:36-55` 只有 18 个槽）。排行条渐变与 Token 副行分隔符都要用，
本批需给 `Palette` 补这两个字段，且 `from_pack` 的映射要跟着补
（对齐 `buildTokenMap`，见 J 批注释）。gpui 渐变用 `gpui::linear_gradient`（`gpui/src/color.rs:765`）。

### 1.6 坑与边界（#17）

1. **账本查询必须丢后台** —— 已有注释写死在 `usage_panel.rs:10-12`：`usage_ledger_query`
   虽是毫秒级纯查询，但**打开连接可能等 `busy_timeout` 最长 5s**，落主线程就是窗口冻住。
   新加的 `usage_ledger_sync(wait=true)`（§1.3 M）更重，同理。
2. **面板常驻 ≠ 定时器该常驻**：`UsagePanel` 实体首次打开后不销毁（`main.rs:441-448`），
   自动刷新必须接可见性开关，范式照 `SessionPanel::set_visible`（`session_panel.rs:174-182`）。
3. **不要把「全 0 成本」当真数据展示**。这是两版共同的红线（`UsageStatsModal.tsx:264-265`、
   `usage_panel.rs:17-19`）。价格未就绪且无旧数据 → 绝不渲染 KPI。
4. **价格建键不做全序择优 = 面板每刷新换个总额**（`modelPricing.ts:12-16`）。
   比较器的最后两级字典序兜底不是洁癖，是正确性。
5. **`input===0 && output===0` 的占位价必须丢**（`:125-127`），收下比查不到更糟。
6. **custom 空串进查询 = 窗口静默退化**（`usageDates.ts:108-114`）。提交闸门是硬要求。
7. **时区**：`tz_offset_minutes` 与 JS `getTimezoneOffset()` **同号（西为正）**
   （`usage_panel.rs:135-138` + 单测 `1112-1117`），反了整体错一天；同时传 IANA 名
   （`iana_time_zone::get_timezone()`，`:403`）让后端按每条记录自身时刻求偏移。custom 分支别漏传。
8. **元素 id 不能用会随语言变的 label**（`UsageRange::key` 的注释 `usage_panel.rs:66-70`）。
   新加的 custom / 自动刷新档位 / 项目下拉项同理：项目下拉用 `project.id` 做 id，**不用 path 也不用 name**。
9. **`with_animation` 持续请求帧**（K 批在滚动条上绕开过）。骨架屏与数字滚动都是短命动画，
   跑完必须停；数字滚动别给五个 KPI 各挂一个。
10. **reduced-motion**：用户机器上系统动画是关的（记忆：`project_reduced_motion_env`），
    但原版**特意豁免了用量面板的数据动效**（`styles.css:465-477`）——照做，别加媒体查询分支。
11. **rustls provider**：见 §K.4 的 ⚠️。首次 HTTPS 请求前要保证 ring provider 已 install。
12. **`byTool`/`byShell`/`byMcp` 是 GPUI 侧多出来的**（原版面板从没渲染过，但 `types.ts` 有类型）。
    重构渲染时**别顺手删掉**（`usage_panel.rs:962-967` 有说明）。

---

## 2. #18 会话面板补全

落点 `crates/mt-app/src/session_panel.rs`。原版 `src/components/SessionList.tsx`（455 行）
+ 纯逻辑 `src/utils/sessionBranch.ts`（174 行）+ `src/utils/sessionJump.ts`（112 行）。

> **审计条目已过时一处**：#18 里列的「品牌图标」M 批已经做完
> （`session_panel.rs:508` 用 `AiVendor::for_session`，`:530-539` 渲染 `BrandIcon`）。
> 本批只需修一处口径差（见 §2.1 的 F 行）。

### 2.1 差异清单

| # | 能力 | 原版 | GPUI 现状 | 判定 |
|---|------|------|-----------|------|
| A | 宿主 + WSL 双来源并行 + 请求序号防串台 | `SessionList.tsx:121-209` | `session_panel.rs:185-250` | ✅ |
| B | 时间格式（刚刚/n分钟前/…/月日/年月日）| `SessionList.tsx:26-50` | `:76-108` | ✅ |
| C | resume 命令白名单 | `utils/aiResume.ts` | `:46-61` + 单测 `:698-738` | ✅ |
| D | 在当前终端 / 新标签恢复 | `SessionList.tsx:76-97,383-393` | `:262-279`（行内按钮 `:582-619`）| ✅ 功能到位 |
| E | 查看正文 + 复制命令 | `SessionViewerModal` + 右键项 | 面板内预览 `:281-433` | 🟡 形态不同，可接受 |
| F | 行图标厂商口径 | **树模式按模型、平铺按 CLI**（`SessionList.tsx:339-342`）| 恒按模型（`:508`）| 🟡 |
| G | 分页加载更多 | `:273-275,444-451` | `:624-640` | ✅ |
| H | 惰性加载（收起时不扫）| React 卸载天然具备 | `visible`/`stale`（`:141-182`）| ✅ |
| I | **平铺⇄树视图切换** | `:242-248,298-304` | 无（config 字段已在，零消费）| ❌ |
| J | **`scan_session_lineage` 分支连线** | `:172-183,256-271` + `sessionBranch.ts` 全文 | 无（mt-ai 已实现，零调用）| ❌ |
| K | **live pane 状态点 + 点击跳转** | `:348-351,366-370,418` + `sessionJump.ts:27-45,66-112` | 无 | ❌ |
| L | **加载 spinner** | `:282-293`（WSL / remote 两个）| 「正在加载 WSL 会话…」纯文本（`:663-670`）| ❌ |
| M | 右键菜单四项 | `:371-402` | 行内按钮（`:578-581` 有注释解释）| 🟡 |
| N | 分支节点标题优先 `branchTitle` | `:268` | 无（依赖 J）| ❌ |
| O | SSH 远程来源 | `:130-150,344-346,430-437` | 无 | **不做**（见 §2.5） |

### 2.2 缺口的实现规格

#### I. 平铺⇄树视图切换

**状态存哪**：`AppConfig::session_list_view: Option<String>`（`crates/mt-config/src/config.rs:194`，
**字段已在磁盘格式里**，default `None`，见 `:500`）。取值 `"flat"` | `"tree"`，
`None`/未知值 = `flat`（原版 `SessionList.tsx:242` 的 `?? 'flat'`）。

**GPUI 落法**：
- `SessionPanel` 持一份运行时视图态，`new()` 时从 `store.read(cx).config().session_list_view` 读入
- 切换动作：改 store 里的 config + `save_config_soon`（新增 setter，
  命名跟随 `set_theme_mode` 一类，`store.rs:1203`）
- **不要每次 render 去读 config** —— `SessionPanel` 已经 `cx.observe(&store, ...)`（`:142-155`），
  读 config 会让每次 store 变化都重算；存本地态、切换时同步即可

**控件**（`SessionList.tsx:298-304`）：头部右侧、刷新按钮左边，等宽字体单字符：
树模式显示 `≡`（点它切回平铺），平铺模式显示 `⑂`（点它切到树）。
tooltip：`t("sessionList","viewFlat")` / `t("sessionList","viewTree")`（**与显示的字符相反**，
文案是「切到 X 视图」）。仅在有活动项目时渲染（`:295`）。

#### J. `scan_session_lineage` 分支连线

**调用参数**（`mt_ai::sessions::scan_session_lineage`，`crates/mt-ai/src/sessions.rs:1793-1796`）：
```rust
scan_session_lineage(
    project_path: String,                        // 与 get_ai_sessions 同一个路径
    bookkept: Option<Vec<BookkeptLineageEdge>>,  // 自记账边
) -> Vec<LineageEdge>
```
`bookkept` 传 `AppConfig::session_lineage`（`Vec<SavedLineageEdge>`，`config.rs:200`）
转成 `BookkeptLineageEdge`（`sessions.rs:1485-1491`）——**两个结构同构但是两个 crate 各持一份，
上层负责转换**（`sessions.rs:1480-1484` 的注释就是这么设计的）。
必须传：Claude 的 CLI fork 不写磁盘指针，这些边的「分叉后第一问」标题**只能由 mt-ai 拿父子文件比对补出**
（`SessionList.tsx:169-171` 注释同义）。

**返回结构**（`LineageEdge`，`sessions.rs:1464-1477`）：
```rust
agent: String                       // "claude" | "codex"
session_id: String                  // 子
parent_session_id: String           // 父
fork_point_uuid: Option<String>     // 仅 Claude 有此精度
branch_title: Option<String>        // 分叉后第一问；None → UI 回落会话标题
```

**发起时机**：与 `get_ai_sessions` **并行**（`SessionList.tsx:172` 与 `:154` 是两个不等待的 invoke），
同一个 `request_id` 守卫（`session_panel.rs:191-193` 已有该机制）；**失败按无分支处理**、
不影响会话列表（`:180-183`）。同样丢 background executor（读一堆文件头，与会话列表同量级）。

**边合并**（`mergeLineageEdges`，`sessionBranch.ts:78-83`）：按 child id 去重，**磁盘优先**
（磁盘指针是 CLI 亲写的权威，自记账只兜文件未落盘的窗口期）。
实现顺序：先塞 bookkept 再塞 disk（后写覆盖）。

**建树**（`buildSessionTree`，`:92-143`）—— 这是纯函数，**照抄并配单测**：
1. 丢弃自指边（`e.parent === e.session`，`:95-96`：留着会让后代的父链游走误判成环）
2. `effectiveParent(id)`（`:102-116`）：父必须**在当前会话列表里**、非自指、
   且**沿父链走到根途中不重逢**（环防御，磁盘数据不可信）。任一不满足 → 该节点按根处理
3. **悬空父按根处理**（父被清理或挤出扫描窗口，不该让子消失）
4. 根**保持输入顺序**（调用方已按时间降序排好）；**子按 timestamp 升序**（先岔的在上，`:138`）

**连线绘制规则**（`flattenSessionTree`，`:159-174`）—— 先根深度优先，与视觉树一致：
```
depth == 0                        → prefix = ""
depth >= 1:
  for i in 0..depth-1:
      prefix += ancestorsLast[i] ? "   " (三个空格) : "│  " (竖线+两空格)
  prefix += ancestorsLast[depth-1] ? "└─ " : "├─ "
```
`ancestorsLast[i]` = 第 i 层祖先是否是其父的**最后一个孩子**。
渲染要求：**等宽字体 + `whitespace-pre` + 不换行 + 不截断**（`SessionList.tsx:405-409`），
颜色 `--text-muted`，`leading-snug mt-0.5`。GPUI 侧用 `.font_family(等宽)` + `.flex_none()`。

**两模式共用同一套行渲染**：平铺模式 `prefix = ""`、`displayTitle = session.title`
（`SessionList.tsx:256-259`，注释「树只是列表长出了结构」）。
树模式 `displayTitle = edge.branch_title.unwrap_or(session.title)`（`:268`，见 N）。

**分页**：树模式的 `slice(0, displayCount)` 是**在拍平之后**做的（`:262`），
不是先截会话再建树 —— 否则父被截掉、子全变成孤儿根。

#### K. live pane 状态点 + 点击跳转

**判定哪个会话是 live**（`findLiveSessionPane`，`sessionJump.ts:27-45`）：
跨**全部项目**扫 pane，三个条件缺一不可：
1. `pane.ai_session.session_id == session_id`
2. `pane.pty_id.is_some()` 且 **`pty_id` 不在 `exitedPtyIds` 里**
3. `pane.status ∈ {AiWorking, AiIdle}`

第 3 条的理由（`:23-26` 注释）：`ai_session` 在 AI 退出后为**续接语义刻意保留**（status 落回 idle），
只看身份会把「claude 已退出的 shell」当成在跑，点击跳过去对着一个死会话。

**GPUI 侧要补的**：
- `AppStore` 新增 `find_live_session_pane(&self, session_id) -> Option<(String /*project_id*/, String /*pane_id*/, PaneStatus)>`。
  跨项目遍历的现成范式：`next_attention_target`（`store.rs:1119-1135`，
  `project_states.iter() → layout.panes()`）
- **`exited_pty_ids` 在 mt-app 里不存在**（grep 零命中；审计第 73 行也记了这条缺失）。
  两个选择：① 本批一并补一个 `HashSet<u32>`（PTY exit 时插入，新建 pane 时清理）——
  更贴原版；② 暂以 `status == Error`（PTY 退出时 store 会把 pane 打成 `Error`）近似，
  因为 `Error` 本就不在第 3 条的白名单里，**实际等价**。
  **推荐 ②**，并在代码里注明「exitedPtyIds 的等价物是 status==Error，见此注释」。
- `PaneStatus`（`crates/mt-app/src/tree.rs:36-42`）→ `mt_ui::icons::StatusKind` 的转换
  已有先例（`ui::status_dot`，`ui.rs:401`），直接用

**点击行为**（`jumpToSession`，`sessionJump.ts:66-112`）：
```
live 存在  → set_active_project(live.project_id) → 下一帧 activate_pane(live.pane_id)
             （原版用 requestAnimationFrame，理由：项目切换后布局才挂到前台；
               GPUI 侧对应 store.set_active_project + store.activate_pane，
               范式见 main.rs:459-464 的 on_jump_attention）
live 不存在 →
  ├─ 会话来自 WSL / 远程        → 提示「无法在本机恢复」，不做任何事
  ├─ agent 无 resume 能力       → 静默不做（branchCapsForAgent 返回 None：opencode/pi）
  └─ 否则                       → 新开终端 + 写 resume 命令 + 回写会话身份
```
新开终端那条的三个细节（`:90-111`）：
- **claude 要先反查 cwd**：`claude --resume` 只认「启动目录」对应的会话桶，
  子目录起的会话在项目根恢复会报 `No conversation found`。
  走 `mt_ai::sessions::lookup_ai_session_cwd`（mt-app 已有调用点 `store.rs:1542`）。
  codex 不按目录分桶、无需反查；grok 按 cwd 分桶但列表只捞「解码目录名全等于项目根」的会话，
  新终端默认目录即正确目录。
- 新 pane 用**返回的那个 pane**，**不能事后再 resolveActivePane**（焦点还没落下去）——
  `session_panel.rs:271-273` 已有同款注释，照办
- **恢复出的会话身份当场写回 pane**（`setPaneAiSessionByPty`，`:106-110`）：
  codex resume 不会重新上报 SessionStart，干等会让新 pane 永远拿不到身份，
  右键的分支入口随之消失。mt-app 侧对应 store 的 pane `ai_session` 写入 + 布局落盘

**分支能力表**（`AGENT_BRANCH_CAPS`，`sessionBranch.ts:35-49`；本批只用 resume 位，
fork 位随 fork 批）：
| agent | fork | resume |
|-------|------|--------|
| claude | `claude --resume {id} --fork-session` | `claude --resume {id}` |
| codex | `codex fork {id}` | `codex resume {id}` |
| grok | **无**（`--resume` 是接管不是复制）| `grok --resume {id}` |
| opencode / pi | 整表缺席（无会话记录）| — |
归一口径（`branchCapsForAgent:57-63`）：codex/grok 显式分流，**其余一律按 claude**
（hook 上报的标识是 `claude-code` 不是 `claude`）。
`session_panel.rs:46-61` 的 `build_resume_command` 已是同一口径 + 同一白名单，直接复用。

**状态点渲染**：`StatusDot`（`SessionList.tsx:418`）放在标题**左边**、同一行
（`:417-422`）。GPUI 用 `mt_ui::icons::StatusDot::new(id, kind)`，
⚠️ **id 必须逐处唯一且跨帧稳定**（`status.rs:230` 的告警）——用
`format!("session-live-{}", session.id)`，不要用行序号。

**树模式行的其它差异**（`SessionList.tsx:356-365`）：
- 树模式整行 `cursor-pointer`，平铺 `cursor-default`
- tooltip：live 时 `t("sessionList","branchTree.runningIn", {project: 项目名})`；
  非 live 时 `"{displayTitle}\n" + t("sessionList","branchTree.clickToResume")`；
  平铺模式恒 `session.title`
- 项目名从 `config.projects` 按 `live.project_id` 反查，查不到用空串（`:349-351`）

#### L. 加载 spinner

原版两个（`SessionList.tsx:282-293`）：
- WSL：`wslLoading` 为真时显示，tooltip `t("sessionList","wslLoading")`
- 远程：`loading && sshConnectionId` 时显示，tooltip `t("sessionList","remoteLoading")`
  → **本批不做**（SSH 不进 crates）

几何：`w-3 h-3`(12px) `border`(1px) 圆环、`border-t-transparent`（缺一段的环）、
`--text-muted` 色、`animate-spin`（Tailwind 默认 1s 匀速线性）。

**GPUI 落法**：自绘（gpui-component 的 `Spinner` 默认图标是空白，见 §0.2）。
用 `mt_ui::icons::vector::Geom::Arc`（`vector.rs:89`）画 270° 弧，
外面套 `.with_animation(id, Animation::new(1s).repeat(), |el, d| el.transform(Transformation::rotate(percentage(d))))`。
现有实现是把 `t("sessionList","wslLoading")` 当**正文**显示（`session_panel.rs:663-670`），
改成图标 + tooltip。

⚠️ `with_animation` 持续请求帧 —— `wsl_loading` 落回 false 时必须让整个元素**从树上消失**。

#### M. 右键菜单（顺带，N 批基建已就绪）

`session_panel.rs:20-22` 的注释写着「gpui 侧还没有上下文菜单基建」——**N 批已经补上了**
（`menu.rs`）。可以把行内的 `↩` / `↗` / `查看` 三个按钮收回右键菜单，与原版一致
（`SessionList.tsx:371-402`），行内只留标题 + 时间 + 徽章，抽屉最窄 240px 的挤压问题随之消失。

菜单项（按顺序，`:378-401`）：
```
查看                                      ← 恒在
──────────                                ← 仅当 canResumeHere
在当前终端恢复                             ← 仅当 canResumeHere
在新终端标签恢复                           ← 仅当 canResumeHere
──────────                                ← 仅当 cmd 拼得出
复制恢复命令                               ← 仅当 cmd 拼得出
```
`canResumeHere = cmd.is_some() && wsl_distro.is_none() && ssh_connection_id.is_none()`（`:377`）。

#### N. 分支节点标题

`displayTitle = edge.branch_title.unwrap_or(session.title)`（`SessionList.tsx:268`）。
理由（同行注释）：fork 是**整份复制**，标题字段连同首条消息一起继承自根会话，
分支之间全同名；真正区分一条分支的是它岔开后干了什么。仅树模式生效。

#### F. 图标口径（小修）

原版两套（`SessionList.tsx:339-342`）：
- **树模式** → `vendorForSession(session)`（按最新模型推厂商，CLI ≠ 模型厂商）
- **平铺模式** → `TYPE_VENDOR[sessionType]`（`:53-57`：`claude→claude`, `codex→openai`, `grok→grok`），
  缺省 `claude`

GPUI 现在恒用 `AiVendor::for_session`（模型优先，`session_panel.rs:508`）→ 平铺模式与原版不一致。
修法：平铺走 `AiVendor::from_session_type(&session.session_type)`
（`brand.rs`，`for_session` 的回落分支就是它，`:123`），树模式保持 `for_session`。

### 2.3 i18n key 清单（#18）

命名空间 `sessionList`，字典源 `src/i18n/locales/sessionList.ts`。**全部已存在双语。**

| key | zh 行 | 用在哪 | 现状 |
|-----|-------|--------|------|
| `time.justNow` / `minutesAgo` / `hoursAgo` / `daysAgo` / `monthDay` | 5-9 | 相对时间 | ✅ 已用 |
| `refresh` | 11 | 刷新 tooltip | ✅ 已用 |
| `loading` | 12 | 首屏加载 | ✅ 已用 |
| `empty` / `selectProject` | 13-14 | 空态 | ✅ 已用 |
| `view` | 15 | 查看 | ✅ 已用 |
| `copyResumeCommand` | 16 | 复制命令 | ✅ 已用 |
| `resumeHere` / `resumeInNewTab` | 17-18 | 两个恢复动作 | ✅ 已用 |
| `loadMore`（`{n}`）| 19 | 加载更多 | ✅ 已用 |
| `wslBadge` | 20 | WSL 徽章 | ✅ 已用 |
| `wslLoading` | 21 | **L：改成 spinner 的 tooltip** | 🟡 现在当正文用 |
| `remoteLoading` | 22 | 远程 spinner | **不做**（SSH）|
| `remoteBadgeTitle`（`{name}`）| 23 | 远程徽章 | **不做**（SSH）|
| `viewFlat` / `viewTree` | 24-25 | **I：切换按钮 tooltip** | ❌ 未用 |
| `branchTree.runningIn`（`{project}`）| 27 | **K：live 行 tooltip** | ❌ 未用 |
| `branchTree.clickToResume` | 28 | **K：非 live 行 tooltip** | ❌ 未用 |
| `branchTree.remoteResumeUnsupported` | 29 | **K：WSL 会话点击时的提示** | ❌ 未用 |

其它命名空间：`panels.sessions`（面板标题，已用）、`sessionViewer.loading`（预览加载中，已用）、
`fileViewer.back`（预览返回，已用）。

⚠️ `branchTree.remoteResumeUnsupported` 在原版是走 **toast**（`pushNotification`，kind `wsl-info`，
`sessionJump.ts:78-84`）。**mt-app 没有 toast 体系**（审计第 71 行记着这条缺口）。
本批用 `prompt::show_alert`（`prompt.rs:213`）代替，并在代码里留 `TODO(toast)` 指向审计条目。

### 2.4 样式要点（#18，取自 `SessionList.tsx` + `styles.css`）

| 元素 | 规格 | 源 |
|------|------|-----|
| 面板根 | `bg-[--bg-surface]` + `select-none` | `:278` |
| 头部 | `px-3 pt-2.5 pb-1.5 text-sm text-[--text-muted] uppercase tracking-[0.12em] font-medium` | `:279` |
| 头部右侧按钮 | `text-xs cursor-pointer hover:text-[--text-primary] transition-colors`；视图切换额外 `font-mono` | `:298-311` |
| 列表容器 | `flex-1 overflow-y-auto px-1.5` | `:316` |
| 会话行 | `flex items-start gap-2 px-2.5 py-1.5 rounded-[--radius-sm] text-xs hover:bg-[--border-subtle] transition-colors` | `:356` |
| 树前缀 | `flex-shrink-0 font-mono whitespace-pre text-[--text-muted] leading-snug mt-0.5` | `:406` |
| 品牌图标槽 | `flex-shrink-0 w-4 h-4 flex items-center justify-center mt-0.5 text-[--text-secondary]`，图标 `size=14` | `:411-412` |
| 标题 | `truncate text-[--text-secondary] group-hover:text-[--text-primary] leading-snug` | `:419` |
| 时间/徽章行 | `text-[--text-muted] text-xs mt-0.5 leading-none`；徽章 `ml-1.5 opacity-70` | `:423-437` |
| spinner | `inline-block w-3 h-3 border border-[--text-muted] border-t-transparent rounded-full animate-spin` | `:284` |
| 加载更多 | `w-full px-2.5 py-2 my-0.5 text-xs text-[--text-muted] text-center rounded-[--radius-sm] hover:bg-[--border-subtle] hover:text-[--text-primary]` | `:446` |
| 空态/加载文案 | `px-2.5 py-3 text-xs text-[--text-muted] text-center` | `:318,322` |

`PAGE_SIZE = 20`（`:23`，`session_panel.rs:38` 已同值）。

### 2.5 坑与边界（#18）

1. **WSL 冷启动秒级阻塞必须丢后台** —— 已有防线，别拆：
   - 模块注释 `session_panel.rs:12-14`：`get_ai_session_content` 与 `get_wsl_ai_sessions`
     原本是 `#[tauri::command(async)]`，靠命令层挪出主线程；现在是普通同步函数
   - 惰性加载：`visible` / `stale` 双标记（`:131-135,174-182`）——
     **收起时项目切换不去扫**（旧版收起时组件根本没挂载）。这是「GPUI 侧已有的惰性加载防线」，
     新加的分支边扫描（J）**必须挂在同一道闸后面**，别绕过 `visible` 直接拉
   - 三个 background spawn：`:212-225`（宿主）、`:230-245`（WSL）、`:295-319`（正文）
   - **新增的 `scan_session_lineage` 同样丢 background executor**，它要读一堆会话文件头
2. **请求序号防串台**：`request_id`（`:129,191-193`）。分支边的回填也要过这道守卫
   （原版 `SessionList.tsx:176-183` 同样用 `reqId`）。
3. **树建好之后才截断分页**（`sessionBranch` 的 slice 在 flatten 之后，`SessionList.tsx:262`）。
   反过来做会把父截掉、子全变孤儿根。
4. **环与悬空父都是磁盘数据的常态**，不是异常：会话文件会被清理、会超出扫描窗口。
   `buildSessionTree` 的两道防御（`:95-96` 自指、`:102-116` 环）必须照抄并配单测。
5. **live 判定不能只看会话身份**（`sessionJump.ts:23-26`）：`ai_session` 在 AI 退出后
   为续接语义刻意保留，status 必须同时在 `{AiWorking, AiIdle}` 里。
6. **`StatusDot` 的 id 要跨帧稳定**（`status.rs:230`）。M 批在项目/pane 上踩过（用 id 拼）。
7. **claude resume 的 cwd 反查**（`sessionJump.ts:90-98`）：漏了就是 `No conversation found`。
   `lookup_ai_session_cwd` 是**同步调用**（J 批已记档理论卡顿），
   在跳转路径上仍要丢 background executor。
8. **恢复后必须当场写回 pane 的 `ai_session`**（`sessionJump.ts:102-110`），
   不能等 hook —— codex resume 不重报 SessionStart。
9. **`session_lineage` 自记账边本批只读不写**：写入端是 fork 动作（pane 右键的「分支会话」），
   属 fork 批。本批只要保证读路径的 `Option<Vec<BookkeptLineageEdge>>` 转换正确，
   空 vec 传 `None` 还是 `Some(vec![])` 都可（`sessions.rs:1798-1800` 两者等价）。
10. **SSH 远程来源：不做，等 mt-ssh 进 crates/**。
    涉及 `ssh_remote_ai_sessions`（`SessionList.tsx:130-150`）、
    `ssh_remote_ai_session_content`（`SessionViewerModal.tsx:73-80`）、
    远程连接名徽章（`:344-346,430-437`）、`remoteLoading` spinner（`:288-293`）。
    `mt_ai::sessions::AiSession` 已经带 `ssh_connection_id: Option<String>` 字段
    （`crates/mt-ai/src/sessions.rs:64`），本机来源恒 `None` ——
    **渲染时按 `None` 处理即可，不要删字段、不要写 `unreachable!`**。
    审计 #18 的括注与 #28 是同一条依赖。

---

## 3. 交付检查单

- [ ] `#17`：C/D/E/F/G/H/I/J/K 九项（点名）+ L~U 十项（顺带）
- [ ] `#18`：I/J/K/L/M/N/F 七项；O 明确标注不做
- [ ] `mt-config` 新增 6 个 usage 偏好字段 + 旧格式互读回归测试（范式：L 批的 locale 两条）
- [ ] `ui::Palette` 补 `color_ai` / `border_strong` + `from_pack` 映射
- [ ] `AppStore` 新增：`find_live_session_pane`、`set_usage_prefs`、`set_session_list_view`
- [ ] `mt-app/Cargo.toml` 新增 HTTP 依赖（推荐 `zed-reqwest` + `blocking`，净新增 crate 0）
- [ ] 纯逻辑单测（中文函数名，对齐既有风格）：
      custom 日期四条边界 / 价格归一全序择优 + 占位价丢弃 + canonical / 建树环与悬空父 /
      连线前缀 / live 判定三条件
- [ ] 既有断言零改动；`cargo test -p mt-app -p mt-config -p mt-usage -p mt-i18n` 全绿
- [ ] 无新增 `TODO(i18n)`（本批所需词条 TS 源头已全有）

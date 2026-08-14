# AI 会话分支树（Session Branch Tree）设计

日期：2026-08-14
状态：设计已与用户确认，进入实施

## 目标

把 AI 会话的分支/派生关系做成 mini-term 的一等公民：

1. **分支动作**：在跑着 AI 的 pane 上一键「分支到新分屏」——新 pane 里是复制出来的会话，原 pane 继续跑，两条思路并排试。
2. **分支树**：AI 历史面板的会话列表支持树形视图（fork 会话挂在父会话下），pane 右键可弹「会话分支」小浮层看当前会话的家族。
3. **节点跳转**：点击树/浮层中的会话节点——该会话已有 pane 在跑则切过去聚焦；没开则新 pane 恢复（resume）它。

**范围明确排除**：会话摘要（另行排期，树节点用现成 `AiSession.title` / 手工命名）；Grok 的 fork 动作（无 CLI 级 fork，仅链路解析预留）。

## 能力位接口（多 agent 预留）

分支能力拆成两个独立能力位，按 agent 声明，UI 按位显隐（沿用 `agent_has_session_log` 的 per-agent 判定模式）：

| agent | 画树（链路解析） | fork 动作（CLI 命令模板） | resume 命令模板 |
|---|---|---|---|
| claude | ✅ 消息级：会话 jsonl 每条复制行带 `forkedFrom: {sessionId, messageUuid}` | `claude --resume {id} --fork-session` | `claude --resume {id}` |
| codex | ⚠️ 官方文档未说明指针，实施时验证会话文件；无指针则依赖自记账（见下） | `codex fork {id}` | `codex resume {id}` |
| grok | ✅ 会话级：`summary.json` 的 parent session reference（树只读，一期不实装解析） | ❌ 菜单隐藏 | ❌ |
| opencode/pi | ❌ 无会话记录，不参与 | ❌ | ❌ |

## 链路数据的两个来源（互补）

1. **磁盘解析**（权威、可回溯历史）：Rust 侧扫项目会话文件提取「子 → 父 + 分叉点」边。Claude 取 jsonl 中首个 `forkedFrom`；Codex 待验证。
2. **自记账**（兜底、即时）：mini-term 自己发起的 fork（右键动作）当场知道 parent→child 关系——写入持久化（config 内 per-project 的 `sessionLineage` 映射）。解决两个问题：① Codex 若无磁盘指针，mini-term 发起的 fork 仍能成树；② 新 fork 的会话文件落盘/hook 上报之前，树上先有节点。磁盘解析与自记账合并时以磁盘为准（磁盘有指针的边去重）。

注意：用户在终端里自己敲 `/branch`、`codex fork` 产生的分支不经过自记账，只靠磁盘解析覆盖（Claude 覆盖得到；Codex 若无指针则这类分支不成树——已知边界，文档标注）。

## 分支动作链路

pane 右键菜单新增「分支到新分屏」（仅当 `pane.aiSession` 存在且 agent 能力位有 fork 模板时显示）：

1. 读当前 pane 的 `aiSession.sessionId` 与 agent；
2. `splitPane` 切出新 pane（复用现有分屏基建，方向取横向，与「向右分屏」一致）；
3. 新 PTY 就绪后 `writePtyInput` 写入 fork 命令（复用重启自动续接/移动端发起会话的「PTY 内核缓冲 stdin，shell 就绪前写入不丢」时序与 sessionId 白名单校验 `/^[A-Za-z0-9_-]+$/`）；
4. 自记账：`{agent, parentSessionId, forkInitiatedAt}` 暂存，新会话 id 由 hook SessionStart 上报后回填成边并持久化（Claude/Codex 的 hook 都会上报新 id；上报前树上以「分支中…」占位）。

新 pane 是新进程，「本会话允许」的权限授权不带过去（官方行为，文档/悬浮提示注明）。

## 树视图（AI 历史面板）

- 列表头部加「平铺 | 树」切换（持久化到 config）；
- 树形态为缩进连线树（制表符风格在 UI 中以缩进 + 连线线条呈现）：根会话顶格，fork 子会话逐层缩进；无分支的会话退化为普通一行；
- 节点行 = 状态点（在跑/空闲/已结束）+ agent 品牌图标 + 标题 + 时间 + 「在跑」时标注所在项目/tab；
- 悬停分支节点显示「岔自父会话第 N 条消息」（仅 Claude 有消息级精度；N 由 `forkedFrom.messageUuid` 在父会话中的序号算出，懒计算）；
- 排序：树内按创建时间，根之间沿用现有列表排序（时间倒序分组）。

## pane 右键「查看会话分支」浮层

只画当前会话所在家族（从根到全部后代），标出「← 当前 pane」；底部动作「从当前分支再岔一条 ⇢ 新分屏」。复用树视图的节点渲染组件，数据取同一棵树的子树。

## 节点点击行为（用户确认的口径）

1. 按 sessionId 在全部项目的 layout 里找 `pane.aiSession.sessionId === id` 且 PTY 活着的 pane → 找到：切项目 + 激活 tab/pane + 聚焦终端（复用 attentionJump 的跳转链路）；
2. 没找到 → 在**该会话所属项目**（cwd 匹配，沿用 ai_sessions 的项目归属逻辑）新开 pane，写入 resume 命令模板恢复；项目不在列表中则提示。

## 数据流

```
Rust: scan_session_lineage(project_path) → [{agent, sessionId, parentSessionId, forkPointUuid?}]
        （Claude 实装、Codex 视验证结果、Grok 预留）
前端: store.sessionLineage 缓存 + 自记账边合并 → buildSessionTree(sessions, edges) 纯函数成树
        （node --test 直测树构建/去重/成环防御）
UI:  SessionList 树视图 / PaneBranchPopover 浮层 → 点击节点 → jumpToSession(sessionId)
```

## 测试

- Rust：lineage 解析单测（含 forkedFrom 缺失、跨文件指针悬空、同 uuid 重复出现的 fork 副本）；
- Node：`buildSessionTree` 纯逻辑直测（多根、多层、悬空父指针落平铺、环防御）；
- 手工验收：Claude/Codex 各 fork 一次走全链路。

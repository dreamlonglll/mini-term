# AI 会话分支树 实施计划

设计：`2026-08-14-session-branch-tree-design.md`（先读它）。分支：`feat/session-branch-tree`。

关键侦查结论（已实证）：
- Claude 指针：会话 jsonl 复制行带 `forkedFrom: {sessionId, messageUuid}`（消息级）；
- Codex 指针：**有**——首行 `session_meta.payload.forked_from_id`（会话级），但 subagent 线程也用它，须按 `payload.thread_source == "subagent"` 过滤；`payload.id` 是自身 id（fork 场景下 `payload.session_id` 是根线程 id，不可用作自身 id）；
- 纯逻辑测试模式：模块加入 `tsconfig.test.json` include → 编译到 `.tmp-tests` → `tests/*.test.cjs` require。

## 任务

1. **Rust `scan_session_lineage`**（ai_sessions.rs + lib.rs 注册）：
   `LineageEdge { agent, session_id, parent_session_id, fork_point_uuid? }`。
   Claude：项目桶逐 jsonl 读前 ~100 行找首个 `forkedFrom` 即止；child = 文件名 stem。
   Codex：复用现有 codex 会话枚举与项目过滤，读首行 meta，跳过 subagent，`forked_from_id != id` 才算边。
   单测（tempdir fixtures：正常边、subagent 跳过、无指针、坏行容错）。
2. **config 字段**：Rust `AppConfig` 加 `session_list_view: Option<String>`、`session_lineage: Option<Vec<SavedLineageEdge>>`（serde default + skip_serializing_if——save_config 强类型反序列化会静默丢未知字段，PR#43 血泪）；前端 types.ts 同步。
3. **前端纯逻辑 `src/utils/sessionBranch.ts`**：`AGENT_BRANCH_CAPS`（claude/codex 的 fork/resume 命令模板，id 白名单校验内置）+ `buildSessionTree(sessions, edges)`（多根、悬空父落平铺、环防御、子按时间升序）。进 tsconfig.test.json，`tests/sessionBranch.test.cjs` 直测。
4. **自记账链路**：store 内存 `pendingForks: Map<ptyId, {agent, parentSessionId}>`；`setPaneAiSessionByPty` 命中 pending → 追加边到 `config.sessionLineage`（按 child id 去重，磁盘可覆盖）+ saveConfig。
5. **分支动作**：`paneActions.forkPaneSession`（splitPane 横切 → 新 pane PTY 就绪写 fork 命令，写入时机复用移动端发起会话的模式）；PaneGroup 右键菜单项，显隐 = `aiSession && caps[agent]`。
6. **树视图**：SessionList 加「平铺|树」切换（`config.sessionListView` 持久化）；树模式 = get_ai_sessions + scan_session_lineage + 自记账边合并 → buildSessionTree → 缩进连线渲染；节点行 = 状态点 + 品牌图标 + 标题 + 时间 + 在跑徽章（按 sessionId 全项目找活 pane）。
7. **节点点击 `jumpToSession`**：有活 pane → 切项目/激活/聚焦（复用 attentionJump 的落点方式）；无 → 当前项目新 tab 写 resume 命令。
8. **pane 浮层**：右键「查看会话分支」→ PaneBranchPopover 画当前家族子树（复用树节点渲染），标「← 当前」，底部「再岔一条 ⇢ 新分屏」。
9. **收尾**：i18n zh/en；CLAUDE.md 命令清单补 `scan_session_lineage`；README ×2 + features ×2 补条目；验证 = cargo test + tsc -p tsconfig.test.json && node --test + tsc --noEmit + 手工清单（Claude/Codex 各 fork 全链路）。

## 边界与已知取舍

- Grok：树数据格式预留（edges 带 agent），解析不实装；fork/resume 菜单对 grok 隐藏。
- 用户手敲 `/branch`、`codex fork` 的分支靠磁盘解析覆盖（Claude/Codex 都覆盖得到，自记账只兜「文件未落盘的窗口期」与万一）。
- 新 pane 新进程,「本会话允许」授权不迁移——菜单 tooltip 注明。

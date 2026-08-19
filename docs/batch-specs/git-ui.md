# 批次规格：Git 全套 UI（缺口 #27）+ 右抽屉悬浮层化

> 产出时间：2026-08-19，由规格提取 agent 逐文件对照 Tauri 原版 `src/` 与已迁移后端 `crates/mt-project/src/git.rs`。
> 对应 `docs/gpui-parity-audit.md:62`（#27「Git 全套 UI」）与 `docs/gpui-parity-audit.md:70`（其他细项「右抽屉应为悬浮层」）。
> **一切以源码实况为准**；每条结论都带源文件与行号，实现时先翻原文再动手。

---

## 0. 摘要与范围纠正

### 0.1 本批要做的组件（6 个 + 1 个容器）

| # | 组件 | 源文件 | 行数 | 角色 |
|---|---|---|---|---|
| A | **RightDrawer** | `src/components/RightDrawer.tsx` | 131 | 悬浮抽屉容器，sessions⇄git 互斥切换 |
| B | **GitHistory** | `src/components/GitHistory.tsx` | 548 | Git 面板容器：仓库栏 + 上下两块可折叠区 |
| C | **GitChanges** | `src/components/GitChanges.tsx` | 442 | 「更改」区：暂存/取消暂存/丢弃/提交 |
| D | **GitHistoryContent** | `src/components/GitHistoryContent.tsx` | 399 | 「提交历史」区：拓扑图 + 分页 |
| E | **DiffModal** | `src/components/DiffModal.tsx` | 262 | 工作区/暂存区单文件 diff（含 `InlineView`/`SideBySideView` 两个导出视图） |
| F | **CommitDiffModal** | `src/components/CommitDiffModal.tsx` | 201 | 某次 commit 的多文件 diff（复用 E 的两个视图） |
| G | **GitWorktreeModal** | `src/components/GitWorktreeModal.tsx` | 833 | worktree 增删/清理/设为项目，**两个入口** |

辅助纯逻辑：`src/utils/gitGraph.ts`（213 行，提交拓扑图布局算法，必须逐条移植）。

### 0.2 ⚠️ 范围纠正：BranchFamilyPanel 不属于本批

`docs/gpui-parity-audit.md:62` 把 `BranchFamilyPanel` 列进 #27，**这是审计的分类错误**。
读 `src/components/BranchFamilyPanel.tsx:1-108`：它是 **AI 会话家族树**（`fetchFamilyRows` → `scan_session_lineage`，
数据是 `FlatSessionRow`/`LineageEdge.branchTitle`，图标是 `BrandIcon`+`StatusDot`，点击走 `jumpToSession`），
和 git 分支毫无关系——是终端右键菜单「查看会话分支」的悬停子面板。

**决议**：本批**不做** BranchFamilyPanel。它属于 fork/会话批（对应 `docs/gpui-parity-audit.md:46` 的
「剩 fork 会话/分支树/SSH 子菜单（随对应功能批）」与 #18 会话面板的 `scan_session_lineage` 分支连线）。
做完本批请顺手把审计 #27 那行的 `BranchFamilyPanel` 划掉，改挂到 #18/#16。

### 0.3 GPUI 现状：git 消费为**零**

grep `crates/mt-app/src/` 全量，git 相关只有三类命中，**没有一处真调用 `mt_project::git`**：

- `crates/mt-app/src/file_tree.rs:291` / `:777`：注释「**跳过「查看变更」** —— git 那套 UI（未建）」，右键菜单里刻意不放该项；
- `crates/mt-app/src/file_tree.rs:678`：注释「git 状态着色（修改/新增/冲突）是后续批次，这里先不传」；
- `crates/mt-app/src/main.rs:587` / `activity_bar.rs:17`：注释「SSH / 移动端 / Git / 更新提醒四个入口 GPUI 侧还没有功能，**不放占位**」。

「worktree 分支名显示」在 GPUI 侧**也不存在**——`crates/mt-app/src/store.rs:686` 出现的 worktree 只是 resume cwd 的注释，
`get_worktree_branches`（`git.rs:1420`）零调用点。项目列表的 ⎇ 徽章仍是 #12 的遗留项。

依赖已就位：`crates/mt-app/Cargo.toml` 已声明 `mt-project.workspace = true`，直接 `use mt_project::git` 即可，**不需要改根 Cargo.toml**。

---

## 1. 后端对照：`crates/mt-project/src/git.rs`（1559 行，已迁移）

### 1.1 UI 用到的全部函数签名与返回结构

Tauri 侧是 `invoke('<command>', {camelCaseArgs})`；GPUI 侧直接函数调用。对照表（左列 = 前端 invoke 名）：

| 前端 command | Rust 函数（`mt_project::git::`） | 签名 | 阻塞级别 |
|---|---|---|---|
| `discover_git_repos` | `discover_git_repos` | `(project_path: &Path) -> Result<Vec<GitRepoInfo>>` | **重**（首次扫盘，深度 5；30s TTL 缓存，`git.rs:294`） |
| `get_changes_status` | `get_changes_status` | `(repo_path: &Path) -> Result<Vec<ChangeFileStatus>>` | **重**（`repo.statuses` 全量 + `recurse_untracked_dirs`） |
| `get_git_log` | `get_git_log` | `(repo_path: &Path, before_commit: Option<&str>, limit: Option<usize>, branch: Option<&str>) -> Result<Vec<GitCommitInfo>>` | 中（revwalk `limit` 条） |
| `get_repo_branches` | `get_repo_branches` | `(repo_path: &Path) -> Result<Vec<BranchInfo>>` | 轻~中 |
| `get_commit_files` | `get_commit_files` | `(repo_path: &Path, commit_hash: &str) -> Result<Vec<CommitFileInfo>>` | 中（tree-to-tree diff） |
| `get_commit_file_diff` | `get_commit_file_diff` | `(repo_path: &Path, commit_hash: &str, file_path: &str, old_file_path: Option<&str>) -> Result<GitDiffResult>` | **重**（LCS O(m·n)） |
| `get_git_diff` | `get_git_diff` | `(project_path: &Path, file_path: &str, staged: Option<bool>) -> Result<GitDiffResult>` | **重**（同上 + 读盘） |
| `git_stage` | `git_stage` | `(repo_path: &Path, files: &[String]) -> Result<()>` | 中（写 index） |
| `git_unstage` | `git_unstage` | `(repo_path: &Path, files: &[String]) -> Result<()>` | 中 |
| `git_stage_all` | `git_stage_all` | `(repo_path: &Path) -> Result<()>` | **重**（`add_all("*")` + 遍历 index） |
| `git_unstage_all` | `git_unstage_all` | `(repo_path: &Path) -> Result<()>` | 中 |
| `git_commit` | `git_commit` | `(repo_path: &Path, message: &str) -> Result<String>` | **走 git CLI，无超时**（`git.rs:1191` 明写「**无超时**，留档见 project_security_audit」） |
| `git_discard_file` | `git_discard_file` | `(repo_path: &Path, files: &[String]) -> Result<()>` | 中（逐文件 statuses + checkout_head） |
| `git_pull` | `git_pull` | `(repo_path: &Path) -> Result<String>` | **CLI，阻塞最多 30s**（`git.rs:1093`） |
| `git_push` | `git_push` | `(repo_path: &Path) -> Result<String>` | **CLI，阻塞最多 30s**（`git.rs:1098`） |
| `list_worktrees` | `list_worktrees` | `(repo_path: &Path) -> Result<Vec<WorktreeInfo>>` | 中（逐 worktree `validate()` + `open_from_worktree`） |
| `add_worktree` | `add_worktree` | `(repo_path: &Path, worktree_path: &str, branch: &str, create_branch: bool, base: Option<&str>) -> Result<String>` | **CLI，阻塞最多 120s**（`git.rs:1355`） |
| `remove_worktree` | `remove_worktree` | `(repo_path: &Path, worktree_path: &str, force: bool) -> Result<String>` | **CLI，阻塞最多 60s**（`git.rs:1397`） |
| `prune_worktrees` | `prune_worktrees` | `(repo_path: &Path) -> Result<String>` | **CLI，阻塞最多 30s**（`git.rs:1405`） |
| `filter_directories` | `mt_project::fs::filter_directories` | `(paths: Vec<PathBuf>) -> Vec<PathBuf>` | 轻（`fs.rs:270`，纯 `is_dir()`） |
| — | `invalidate_repo_cache` | `() -> ()` | 轻（`git.rs:298`；worktree 三个 CLI 函数**成功后内部已自调**，UI 不必再调） |

**返回结构**（全部 `#[serde(rename_all = "camelCase")]`，字段名与 `src/types.ts` 一一对应）：

```rust
GitRepoInfo   { name: String, path: PathBuf, current_branch: Option<String>, is_worktree: bool }        // git.rs:232
ChangeFileStatus { path: String, old_path: Option<String>,
                   staged_status: Option<GitStatus>, unstaged_status: Option<GitStatus>,
                   status_label: String }                                                              // git.rs:52
GitFileStatus { path: String, old_path: Option<String>, status: GitStatus, status_label: String }       // git.rs:43
GitCommitInfo { hash, short_hash, message, body: Option<String>, author, timestamp: i64,
                parent_hashes: Vec<String> }                                                            // git.rs:242
BranchInfo    { name: String, is_head: bool, is_remote: bool, commit_hash: String }                     // git.rs:263
CommitFileInfo{ path: String, status: String /* added|deleted|modified|renamed */, old_path: Option<String> } // git.rs:255
GitDiffResult { old_content: String, new_content: String, hunks: Vec<DiffHunk>,
                is_binary: bool, too_large: bool }                                                      // git.rs:81
DiffHunk      { old_start, old_lines, new_start, new_lines: u32, lines: Vec<DiffLine> }                 // git.rs:62
DiffLine      { kind: String /* "context"|"add"|"delete" */, content: String,
                old_lineno: Option<u32>, new_lineno: Option<u32> }                                      // git.rs:72
WorktreeInfo  { name: String, path: String, branch: Option<String>,
                is_main: bool, is_valid: bool, is_locked: bool }                                        // git.rs:1262
GitStatus     enum { Modified, Added, Deleted, Renamed, Untracked, Conflicted }                         // git.rs:32
```

⚠️ `WorktreeInfo` 只 `Serialize` 不 `Deserialize`（`git.rs:1260`），Rust 侧直接用结构体不受影响。

### 1.2 阻塞调用清单——**必须丢 `cx.background_executor()`**

`git.rs:10-14` 的模块头注释是硬约束原文：

> 网络类操作（pull/push/worktree add/remove/prune）走 git CLI 而非 git2 …… 原实现靠
> `#[tauri::command(async)]` 把阻塞等待挪出主线程，现在这一层不做线程调度，
> **调用方必须自己放到后台执行器上跑**（GPUI 的 `background_executor`），
> 否则 30s/120s 的 `recv_timeout` 会卡住 UI 线程。

同一条也记在进度文档 `docs/gpui-migration-progress.md:114`（技术债段）与 `:25`（C 批验收记录）。

**必须丢后台的（超时会真的把主线程钉死）**：
`git_pull`(30s) · `git_push`(30s) · `add_worktree`(120s) · `remove_worktree`(60s) · `prune_worktrees`(30s) · `git_commit`(**无超时**)

**强烈建议丢后台的（git2 同步 IO，大仓库上百毫秒起）**：
`discover_git_repos`（首次扫盘深度 5）· `get_changes_status` · `get_git_log` · `get_repo_branches` ·
`get_commit_files` · `get_commit_file_diff` · `get_git_diff` · `git_stage_all` · `git_unstage_all` ·
`git_discard_file` · `list_worktrees`

**可留主线程**：`filter_directories`、`invalidate_repo_cache`。

**现成范式**（照抄 `crates/mt-app/src/file_tree.rs:138-156`）：

```rust
let task_repo = repo_path.clone();
cx.spawn(async move |this, cx| {
    let result = cx
        .background_executor()
        .spawn(async move { mt_project::git::get_changes_status(&task_repo) })
        .await;
    let _ = this.update(cx, |view: &mut GitChanges, cx| {
        view.loading = false;
        match result {
            Ok(list) => view.changes = list,
            Err(err) => eprintln!("[git] 取变更失败: {err:#}"),
        }
        cx.notify();
    });
})
.detach();
```

### 1.3 后端行为要点（影响 UI 判断）

- **仓库发现口径**（`git.rs:319-391`）：先试 `discover_repo_limited`（向上最多 5 级找 `.git`），命中就**只返回这一个**并 `return`；
  没命中才向下扫子目录（`MAX_DEPTH=5`，跳过 `.git`/`node_modules`/`target`/`.next`/`dist`/`__pycache__`/`.superpowers`）。
  项目目录**外**的关联 worktree 不注入（`git.rs:329-330` 注释：worktree 靠「设为项目」拥有自己的 Git 面板）。
- **发现缓存**：`REPO_PATH_CACHE` TTL 30s（`git.rs:294`）。worktree 三个 CLI 成功后自动 `invalidate_repo_cache()`。
  ⚠️ **UI 层不要再自己缓存 repos 列表**（原版前端另有一层 `projectDataCache`，见 §4.2.3）。
- **detached HEAD**：`discover_git_repos` 把 `current_branch` 填成 `"(1a2b3c4)"` 带括号的短 hash（`git.rs:484-489`）。
  UI 侧照原样显示，**不要**当分支名去 `get_git_log(branch=...)`——那会走 `refs/heads/(1a2b3c4)` 查找失败并 `bail!("未找到分支:…")`。
- **空仓库**（`repo.head().is_err()`）：`WT_NEW` 映射成 `Added` 而非 `Untracked`（`git.rs:110-114`）；
  `git_unstage`/`git_unstage_all` 走「直接清 index」分支（`git.rs:1135-1142`、`:1180-1186`）。
- **diff 上限**：`MAX_DIFF_BYTES = 1_048_576`（1MB，`git.rs:714`）→ `too_large: true`；
  `MAX_LCS_CELLS = 10_000_000`（`git.rs:717`）→ 超过就退化成 `full_replace_diff`（整块删+整块加，仍返回 `too_large: false`）。
- **二进制**：`blob.is_binary()` 或 UTF-8 解码失败 → `is_binary: true`，`hunks` 为空。
- **上下文行数**：`CONTEXT = 3`（`git.rs:818`）。
- **`git_discard_file` 的 untracked 判定必须开 `include_untracked`**（`git.rs:1227-1229` 有整段注释，是历史 bug 的修复记录）——迁移时别把这段"优化"掉。
- **`run_git_command` 的前置校验**（`git.rs:1045-1051`）：非目录、或 `repo_path/.git` 不存在直接 `bail!` 中文错误。
  worktree 目录下 `.git` 是**文件**，用的是 `.exists()` 不是 `.is_dir()`——别改。

---

## 2. 【单列一节】右抽屉悬浮层化

### 2.1 原版语义（`src/components/RightDrawer.tsx` + `src/App.tsx:492-545`）

**结构位置**：抽屉**不在** Allotment 里，而是 Allotment 的**兄弟节点**，挂在一个 `relative` 容器内（`App.tsx:494`）：

```
<div className="relative flex-1 overflow-hidden">      ← App.tsx:494
  <Allotment>                                          ← App.tsx:495 两栏：中间栏 | 终端栏
    <Allotment.Pane visible={middleColumnVisible}>…</Allotment.Pane>
    <Allotment.Pane>TerminalArea…</Allotment.Pane>
  </Allotment>
  <RightDrawer initialWidth={config.rightDrawerWidth ?? 340} … />   ← App.tsx:540-543
</div>
```

**抽屉本体样式**（`RightDrawer.tsx:64-72`）：

```
absolute top-0 right-0 h-full z-[45]
flex flex-col
bg-[var(--bg-overlay)]  border-l border-[var(--border-default)]
shadow-[var(--shadow-overlay)]
class "overlay-drawer"（+ "is-closing"）
style={{ width }}
```

**关键点：终端不让位**。抽屉 `absolute` 浮在终端之上，终端宽度**不变**、**不重排**、**不 resize PTY**。
这是与现在 GPUI「第三栏 resizable」（`crates/mt-app/src/main.rs:547-553`）最大的语义差：现在开抽屉会挤窄终端并触发一次 PTY resize。

**z-index 分层**（`RightDrawer.tsx:65-66` 原注释）：
- 抽屉 `z-45` —— 必须压过 allotment 分隔条的 `z-index:35`，否则那根线画在抽屉上面；
- 低于弹窗 `z-50` —— 抽屉开着时弹窗仍在最前。

GPUI 对应：抽屉层放在 `columns_group` **之后**、`usage_layer` 与 `Root::render_dialog_layer` **之前**（见 `main.rs:735-780` 的 child 顺序），
并加 `.occlude()`（照 `main.rs:697` 的 usage_layer）拦住穿透到终端的鼠标事件。

**宽度**：
- `MIN_WIDTH = 240`，`MAX_WIDTH = 720`（`RightDrawer.tsx:8-9`），`clamp` 在 `:17`；
- 拖拽期间宽度**自持**在组件 state，**松手才回调** `onResizeEnd` 落盘（`RightDrawer.tsx:49-56`、`App.tsx:467-472`）；
- 左缘手柄：`absolute left-0 top-0 h-full w-1.5 -translate-x-1/2 cursor-col-resize hover:bg-[var(--accent)]/40 z-10`（`RightDrawer.tsx:74-77`）；
- 拖拽方向：抽屉贴右缘，**左缘往左拖 = 变宽**，故 `startWidth + (startX - currentX)`（`RightDrawer.tsx:47-48`）；
- 拖拽期间 `document.body.style.userSelect = 'none'`（防划过终端误选，`:44-45`）——GPUI 侧不需要，但等价的「拖拽中不派发终端选择」要注意；
- **默认值不一致**：原版 `?? 340`（`App.tsx:541`），GPUI 现状 `unwrap_or(320.0).clamp(240.0, 720.0)`（`crates/mt-app/src/store.rs:1334`）。
  本批**改成 340** 对齐原版（clamp 区间已经对）。

### 2.2 动画（`src/styles.css`）

| 场景 | 类名 | 定义 |
|---|---|---|
| 进场 | `.overlay-drawer` | `animation: drawerSlideIn var(--motion-overlay-in) var(--ease-overlay-in)` （`styles.css:335-341`） |
| 退场 | `.overlay-drawer.is-closing` | `animation: drawerSlideOut var(--motion-overlay-out) var(--ease-overlay-out) forwards; pointer-events: none` （`styles.css:343-346`） |
| 换面板 | `.panel-swap-in` | `animation: panelSwapIn var(--motion-terminal-swap) var(--ease-overlay-in)` （`styles.css:356-358`） |
| 段控件选中块 | `.drawer-tab-indicator` | `transition-transform`，`transitionDuration: var(--motion-tab-indicator)` （`RightDrawer.tsx:88-95`） |

keyframes（`styles.css:284-313`）：
```css
drawerSlideIn  { from { transform: translateX(100%) } to { transform: translateX(0) } }
drawerSlideOut { from { transform: translateX(0) }    to { transform: translateX(100%) } }
panelSwapIn    { from { opacity:0; transform: translateX(10px) } to { opacity:1; transform: translateX(0) } }
```

时长常量（`styles.css:67-78`）：
`--motion-overlay-in: 0.24s` · `--motion-overlay-out: 0.14s` · `--motion-terminal-swap: 0.2s` ·
`--motion-tab-indicator: 0.22s` · `--motion-section-toggle: 0.22s` ·
`--ease-overlay-in: cubic-bezier(0.16, 1, 0.3, 1)` · `--ease-overlay-out: cubic-bezier(0.4, 0, 0.9, 0.6)`

**⚠️ reduced-motion 豁免**：`styles.css:424-451` 把 `.overlay-drawer` / `.overlay-drawer.is-closing` /
`.panel-swap-in` / `.drawer-tab-indicator` 显式列在通配 `prefers-reduced-motion` 之外——
浮层进出场**照常播完整动画**。用户机器 reduced-motion 为 `reduce`（见记忆 `project_reduced_motion_env`），
GPUI 侧若引入减弱动效判定，必须照抄这条豁免。

**退场 DOM 留存**：`useOverlayPresence(rightDrawer !== null)` + `useOverlayValue(rightDrawer)`（`RightDrawer.tsx:31-33`），
`OVERLAY_EXIT_MS = 400`（`src/hooks/useOverlayMotion.ts:19`）。
关闭时 DOM 与**面板内容**都多留 400ms，否则"抽屉在滑出的同时内容先空掉"（`RightDrawer.tsx:29-30` 原注释）。
GPUI 侧若用 `with_animation` 或手写补间，需保证退场期间面板实体仍在树上。

### 2.3 sessions⇄git 互斥切换：状态存哪

**存 store 的运行时字段，不落盘**（`src/store.ts:684-689`，原注释一字不改地照抄）：

```ts
// 右侧悬浮抽屉（Sessions / Git）——运行时态,互斥单抽屉,不持久化开合(每次启动收起)
rightDrawer: 'sessions' | 'git' | null;          // store.ts:685   初值 null（store.ts:750）
toggleRightDrawer: (panel) => void;              // store.ts:686 / 实现 :1259-1260
openRightDrawer: (panel) => void;                // store.ts:688 / 实现 :1262
closeRightDrawer: () => void;                    // store.ts:689 / 实现 :1264
```

语义差别（**别写反**）：
- `toggleRightDrawer(p)`：`rightDrawer === p ? null : p` —— ActivityBar 两颗按钮用它（`ActivityBar.tsx:135`、`:145`），再点一次关闭；
- `openRightDrawer(p)`：直接 `set({ rightDrawer: p })` —— 抽屉内 segmented 切换用它（`RightDrawer.tsx:107`），
  **不做「再点一次关闭」**（`store.ts:687` 原注释）。

只有**宽度**落盘：`config.rightDrawerWidth?: number`（`src/types.ts:51`；Rust 侧 `crates/mt-config/src/config.rs:154` `right_drawer_width: Option<f64>` 已在）。

**GPUI 现状与改法**：`crates/mt-app/src/main.rs:174` 现在只有 `sessions_open: bool`，
`toggle_sessions`（`main.rs:428-434`）里还要 `session_panel.set_visible(open, cx)`
（收着时不去扫会话，WSL 那一路会冷启动整台 VM——这条**必须保留**）。

改法：`sessions_open: bool` → `right_drawer: Option<DrawerPanel>`（enum `Sessions | Git`）：
- `toggle_drawer(panel)`：相同则 `None`，否则 `Some(panel)`；
- `open_drawer(panel)`：直接 `Some(panel)`；
- 每次变更后同步 `session_panel.set_visible(matches!(self.right_drawer, Some(Sessions)), cx)`；
- Git 面板同理要有 `set_visible`——收着时**不要**跑 `discover_git_repos`（同样是扫盘）；
- `main.rs:576` 的 `if sessions_open && let Some(w) = sizes.get(2)` 那段随第三栏一起删（宽度改由抽屉自己的拖拽手柄回写）。

### 2.4 抽屉标题条（`RightDrawer.tsx:80-124`）

高度 `h-9`（36px），`px-1.5`，`border-b border-[var(--border-subtle)]`，`flex-shrink-0`。

1. **segmented 切换器**（`role="tablist"`，`flex-1`，`rounded-[var(--radius-sm)]`，`border border-[var(--border-default)]`，`overflow-hidden`）：
   - 选中态底块是一个**滑动的绝对定位 span**（`RightDrawer.tsx:88-95`）：
     `absolute inset-y-0 left-0 w-1/2 bg-[var(--accent-subtle)]`，
     `transform: translateX(panel === 'git' ? '100%' : '0%')`，两个 tab 等宽所以位置只有 0/100% 两种；
   - 两个 tab 按钮：`relative flex-1 px-2 py-1 text-xs`；选中 `text-[var(--accent)] font-medium`，
     未选中 `text-[var(--text-muted)] hover:text-[var(--text-primary)]`；
   - 文案：`t('panels.sessions')` / `t('panels.git')`。
2. **关闭按钮**：`w-6 h-6`，`rounded-[var(--radius-sm)]`，hover `bg-[var(--border-subtle)]`，
   内嵌 11×11 的 ✕ SVG（`viewBox="0 0 16 16"`，`strokeWidth=1.6`，`strokeLinecap="round"`，
   path `M4.5 4.5l7 7M11.5 4.5l-7 7`，`RightDrawer.tsx:120-122`）；
   title/aria = `t('app.activityBar.closeDrawer')`。
3. **内容层**：`<div key={panel} className="flex-1 min-h-0 overflow-hidden panel-swap-in">` ——
   `key={panel}` 是刻意的：换面板时这层**重建**，横向淡入动画才会重播（`RightDrawer.tsx:125` 原注释）。
   GPUI 对应：换面板时给容器换 `ElementId`（或重建子实体）以重启动画。

### 2.5 ActivityBar 的 Git 入口（`src/components/ActivityBar.tsx:143-150`）

现在 GPUI 边条按 `main.rs:587` 刻意**没放** Git 占位。本批要补：

- 按钮位置：紧跟 Sessions 按钮之后、分隔线（`w-6 h-px bg-[var(--border-default)] my-1`）之前；
- `onClick` → `toggleRightDrawer('git')`；`title` = `t('app.activityBar.git')`（zh「Git 变更」/ en「Git changes」，已在字典 `crates/mt-i18n/src/dict.rs:24`/`:68`）；
- 选中态挂 `ACCENT_BAR`（accent 竖条，GPUI 侧已有 `activity_bar::strip_button` 的 active 参数）；
- **图标 SVG 逐点照抄**（`ActivityBar.tsx:24-31`，`viewBox="0 0 16 16"`，`fill=none stroke=currentColor strokeWidth=1.2 strokeLinecap/Join=round`，18×18）：
  ```
  <circle cx="5"  cy="4"  r="1.5" />
  <circle cx="11" cy="4"  r="1.5" />
  <circle cx="5"  cy="12" r="1.5" />
  <path d="M5 5.5v5M11 5.5v1a2 2 0 01-2 2H5" />
  ```
  ⚠️ 走 `mt_ui::VectorIcon` DSL，**不要**用 gpui-component 的 `IconName`——
  它无 svg 资产会渲染空白且编译期无感（`docs/gpui-migration-progress.md:16` M 批记档）。

---

## 3. 组件 B：GitHistory（Git 面板容器）

源：`src/components/GitHistory.tsx:102-548`

### 3.1 布局与控件顺序（自上而下）

```
<div data-panel h-full bg-[var(--bg-surface)] flex flex-col
     border-t border-[var(--border-subtle)] select-none>            ← :351
  ① 仓库栏（repos.length > 0 时才渲染）  h-[34px] pl-1.5 pr-2 border-b  ← :353-499
     ├ 仓库下拉触发器（▾ + 名称 + ⎇ 徽章）  text-[var(--color-folder)]  ← :356-377
     ├ 仓库下拉面板（absolute left-1.5 right-2 top-full z-50）        ← :378-403
     ├ 分支徽章 + 分支下拉（displayBranch 存在时）                     ← :407-472
     ├ <div className="flex-1" />（占位推右）                          ← :474
     ├ 刷新 ↻ 按钮  w-5 h-5                                          ← :476-485
     ├ GitActionButton pull                                          ← :486-491
     └ GitActionButton push                                          ← :492-497
  ② SectionHeader「更改」 h-[30px]（无上边框）                          ← :502
  ③ GitChanges 容器 .git-section-body                                ← :503-513
  ④ 中缝拖拽手柄（两块都展开时）h-0 wrapper + absolute h-1.5           ← :516-523
  ⑤ SectionHeader「提交历史」 h-[30px] bordered（有上边框）             ← :526
  ⑥ GitHistoryContent 容器 .git-section-body                         ← :527-539
  ⑦ GitWorktreeModal（repoPath=worktreeRepo）                        ← :541-545
```

### 3.2 空态与前置分支

按顺序短路（`GitHistory.tsx:331-345`）：

1. **无激活项目** → 整块居中 `t('gitHistory.selectProject')`，
   `h-full bg-[var(--bg-surface)] flex items-center justify-center text-[var(--text-muted)] text-base`；
2. **SSH 远程项目**（`!!project?.sshConnectionId`，`:108`）→ 居中 `t('gitHistory.remoteNotSupported')`，
   同上样式 + `border-t border-[var(--border-subtle)]`。原注释：git 命令跑在本地，对远程路径无意义（远程 Git 二期）；
3. **无仓库**（`repos.length === 0`）→ 仓库栏整条**不渲染**，两个 SectionHeader 仍在；
   「提交历史」区内由 GitHistoryContent 显示 `t('gitHistoryContent.noRepos')`（因为 `repoPath` 为空串）。

### 3.3 数据流与状态

**模块级（非组件 state）视图状态**（`GitHistory.tsx:13-15`，原注释「抽屉一关本组件就整个卸载，放模块级让它在会话内保留；**有意不落盘**——属于临时视图状态」）：

```ts
const sectionUi = { changesOpen: true, historyOpen: true, ratio: 0.5 };
const clampRatio = (r) => Math.min(0.85, Math.max(0.15, r));   // :17
```

GPUI 侧对应：放 `thread_local`/静态（照 `crates/mt-app/src/overlay.rs` 与 `ui.rs` 的 `thread_local` 先例），**不写进 config**。

**仓库列表**：
- 初值来自会话级缓存 `getGitHistoryCache(project.path)`（`src/utils/projectDataCache.ts:33-38`，`Map<projectPath, {repos, selectedRepo}>`）；
- 项目切换 effect（`:207-223`）：先吃缓存（无缓存则清空）→ 关下拉 → `loadRepos()`；
- `loadRepos`（`:182-195`）：`discover_git_repos(project.path)` →
  选中仓库保持原值（若仍在列表里）否则取 `r[0]?.path ?? ''` → 回写缓存；失败 → `setRepos([])`；
- **远程项目直接 return**（`:183`）。

**分支列表**：
- `loadBranches(repoPath)`（`:197-205`）：`get_repo_branches` → **迟到响应丢弃**：
  `if (selectedRepoRef.current === repoPath) setBranches(b)`（`:201`，换仓库后的旧响应不许覆盖）；
- 换仓库 effect（`:226-233`）：`branches=[]`、`viewBranch=undefined`、关下拉、`pullState/pushState=null`、重载分支。

**viewBranch 语义**（`:173-174` 原注释）：正在查看（**未 checkout**）的分支；`undefined` = 跟随 HEAD。
**只改历史显示，不做 checkout**。`displayBranch = viewBranch ?? selectedRepoInfo?.currentBranch`（`:347`），
`isViewingOther = viewBranch !== undefined && viewBranch !== currentBranch`（`:348`）。

**刷新联动**：
- `onCommitSuccess`（`:252-255`）：`historyRefreshKey++`（强制 GitHistoryContent 整体重建）+ 重载分支（分支头已前移）；
- `refreshRepoMeta`（`:258-261`）：`loadRepos()` + 重载分支——由 GitHistoryContent 的 pty-output 嗅探回调进来；
- 刷新按钮（`:479-482`）：`refreshRepoMeta()` + `historyRefreshKey++`。

⚠️ `key={historyRefreshKey}`（`:533`）会**整体重建** GitHistoryContent：滚动位置、已加载的分页全部丢弃回到第一页。这是原版行为，照抄。

### 3.4 仓库下拉（`:355-404`）

- 触发器：`▾`（`&#9662;`）旋转 `0deg`/`-90deg`（`transition-transform duration-150`）+ 仓库名（`truncate font-medium text-sm`）
  + worktree 时追加 `⎇`（`text-sm text-[var(--text-muted)]`，title = `t('gitHistoryContent.worktreeBadgeTitle')`）；
  整体色 `text-[var(--color-folder)]`，hover `bg-[var(--border-subtle)]`，`title = selectedRepoInfo?.path`；
- 面板：`absolute left-1.5 right-2 top-full z-50 mt-0.5`，`bg-[var(--bg-elevated)] backdrop-blur-[12px]`，
  `border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] overflow-hidden`；
- 行：`px-3 py-1.5 text-sm`；选中 `bg-[var(--accent-subtle)] text-[var(--accent)]`，
  未选中 `text-[var(--text-primary)] hover:bg-[var(--border-subtle)]`；
  行尾若有 `currentBranch` 显示胶囊 `text-sm leading-[18px] px-1.5 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]`，
  worktree 前缀 `⎇ `；
- 点击行：`setSelectedRepo` + 关下拉 + **立即回写缓存**（`:391`）；
- **点外关**：`document` 的 `mousedown` 监听，判 `dropdownRef.contains(e.target)`（`:236-248`）。
  GPUI 侧用 `menu.rs` 的全窗遮罩点外关同款做法。

### 3.5 分支徽章与下拉（`:407-472`）

- 徽章：`inline-flex items-center gap-0.5 text-sm leading-[18px] px-1.5 rounded font-mono cursor-pointer`
  - 正在看别的分支：`text-[var(--color-accent,#58a6ff)] bg-[rgba(88,166,255,0.15)] hover:bg-[rgba(88,166,255,0.25)]`
  - 跟随 HEAD：`text-[var(--text-muted)] bg-[var(--border-subtle)] hover:bg-[var(--color-accent,#58a6ff)] hover:text-white`
  - 内容：分支名 `truncate max-w-[140px]` + `▾`（`text-[0.7rem] opacity-70`）
  - title：`gitHistoryContent.viewingBranchHistory`（带 `{branch}`/`{head}` 插值）或 `gitHistoryContent.switchBranchHint`
- 点击：分支列表为空时**懒加载**一次（`:422`），再 toggle 下拉；
- 下拉面板：`absolute top-full left-0 mt-0.5`，`min-w-[180px] max-h-[320px] overflow-y-auto`，
  `bg-[var(--bg-elevated)] border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)]`，
  **`style={{ zIndex: 100 }}`**（比仓库下拉的 z-50 更高）；
- 行：`px-3 py-1.5 text-xs`；左侧 1.5×1.5 圆点——远程 `var(--text-muted)`，本地 `rgb(63, 185, 80)`；
  分支名 `truncate font-mono flex-1`；若 `b.name === currentBranch` 追加 `HEAD` 胶囊
  （`text-[0.7rem] px-1 rounded bg-[var(--color-accent,#58a6ff)] text-white font-medium`）；
- 加载中显示 `t('gitHistoryContent.loading')`；
- 点行 → 关下拉 + `setViewBranch(b.name)`（**不 checkout**）。

### 3.6 pull / push 按钮（`GitActionButton`，`:21-67`）

`SyncState = { status: 'loading'|'success'|'error', error?: string } | null`（`:19`）

| 态 | 字形 | 颜色 |
|---|---|---|
| loading | `↻` | `text-[var(--text-muted)]` + `animate-pulse` |
| success | `✓` | `text-[var(--color-success)]` |
| error | `✕` | `text-[var(--color-error)]` |
| 常态 | pull=`↓` / push=`↑` | `text-[var(--text-muted)] hover:text-[var(--text-primary)]` |

- 尺寸 `w-5 h-5 flex items-center justify-center text-sm rounded flex-shrink-0`；
- `disabled` = 任一方 loading（两个按钮互斥禁用，`:489`/`:495`）；disabled 时 `opacity-50 cursor-not-allowed`；
- title：error 时是**错误全文**，否则字面量 `'Git Pull'`/`'Git Push'`（**原版这两个字符串没进 i18n**，照抄硬编码）；
- `handlePull`（`:263-277`）：置 loading、清对方状态 → `git_pull` → 成功则 `loadBranches` + `historyRefreshKey++`；
  失败置 `{status:'error', error: String(e)}`；**无论成败 1500ms 后清回 null**（`:276`）；
- `handlePush`（`:279-292`）：同构，但成功后**只**重载分支（不刷历史）。
- ⚠️ `git_pull`/`git_push` 是 30s 阻塞 CLI → **必须后台执行器**。

### 3.7 仓库栏右键菜单（`:300-329`）

在**仓库下拉触发器**上右键（`onContextMenu={handleRepoContextMenu}`，`:360`），两项 + 一条分隔：

1. `t('gitHistoryContent.openInTerminal')` → `newTerminal(projectId, undefined, opts)`：
   - **项目根仓库不带 cwd 覆盖**（默认就是项目根）；子仓库/worktree 才带
     `{ cwd: repo.path, title: repo.isWorktree ? `⎇ ${repo.currentBranch ?? repo.name}` : repo.name }`（`:313-318`）；
   - 尾部分隔符归一化后比较：`repo.path.replace(/[\\/]+$/,'') === projectPath.replace(/[\\/]+$/,'')`；
2. `separator`；
3. `t('gitHistoryContent.manageWorktrees')` → `setWorktreeRepo(repo.path)` 打开 GitWorktreeModal（`discoverRepos` **不传**，即单仓库模式）。

GPUI 侧：走已建好的 `crates/mt-app/src/menu.rs`（`menu::show` + `menu::item` + `menu::separator`）。
`newTerminal` 对应 `AppStore::new_terminal`（`store.rs:372`）——⚠️ **它现在不接受 cwd/title 覆盖**
（`store.rs:381` 硬传 `None` 给 `spawn_pane`）。本批需要给它加 `cwd_override: Option<String>` 与 `title: Option<String>` 参数，
或另开 `new_terminal_at`。`spawn_pane`（`store.rs:800-812`）本身已支持 `cwd_override`，只差把口子开到 `new_terminal`。

### 3.8 两块折叠区与中缝拖拽

**SectionHeader**（`:69-100`）：
`flex items-center gap-1 px-2 h-[30px] flex-shrink-0 cursor-pointer text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors duration-100`；
`bordered` 时加 `border-t border-[var(--border-subtle)]`（**只有下方「提交历史」用**）。
左侧 chevron `▾`（`&#9662;`）：`class="git-section-chevron text-base w-3 text-center text-[var(--text-muted)]"`，
`transform: rotate(0deg)`（展开）/ `rotate(-90deg)`（折叠）。标签 `text-sm font-medium`。

**区块体**（`:503-513`、`:527-539`）：
```
className="git-section-body min-h-0 overflow-hidden" + (resizing ? " is-resizing" : "")
style={{ flexGrow: open ? (otherOpen ? ratio : 1) : 0, flexBasis: 0 }}
```
`.git-section-body { transition: flex-grow var(--motion-section-toggle) var(--ease-overlay-in) }`（`styles.css:363-365`），
`.is-resizing { transition: none }`（`:367-369`）。
原注释（`styles.css:360-362`）：区块 `flex-basis` 恒为 0，高度全由 grow 决定，两块同时补间时总和不变、不会跳。

⚠️ **两块常驻挂载**（`GitHistory.tsx:111-112` 原注释）：折叠只把高度收到 0，
**已加载的 commits、提交草稿都不丢**。GPUI 侧不要用 `when()` 把子实体摘掉。

**中缝拖拽**（`:135-157`、`:516-523`）：
- 只在两块**都展开**时渲染；结构是 `relative h-0 flex-shrink-0 z-30` 的零高包裹 + 内部
  `absolute left-0 right-0 -top-[3px] h-1.5 cursor-row-resize hover:bg-[var(--accent)]/40`；
- 按下时记两块内容的**实测 offsetHeight**（`h1`、`total = h1 + h2`），移动量换算 `clampRatio((h1 + dy) / total)`；
- `total <= 0` 直接 return；拖拽期间 `body.userSelect='none'` + `setResizing(true)`。

---

## 4. 组件 C：GitChanges（「更改」区）

源：`src/components/GitChanges.tsx:86-442`。Props：`{ projectPath（当前未用，`_projectPath`）, repoPath, onCommitSuccess }`。

### 4.1 布局（自上而下，`:359-441`）

```
<div h-full flex flex-col>
  ① 工具栏  flex items-center justify-between px-3 py-1.5 flex-shrink-0     ← :362-377
     ├ 左：刷新 ↻   text-sm  title=t('gitChanges.refresh')
     └ 右：视图切换  text-xs  list时显示 '⊞' / tree时显示 '≡'
  ② 文件列表 flex-1 overflow-y-auto px-1                                    ← :380-401
     ├ loading && changes.length===0 → t('gitChanges.loading')
     ├ !loading && changes.length===0 → t('gitChanges.empty')
     ├ 分组「已暂存」  action = t('gitChanges.unstageAll') → handleUnstageAll
     ├ 分组「未暂存」  action = t('gitChanges.stageAll')  → handleStageAll
     └ 分组「未跟踪」  action = t('gitChanges.stageAll')  → handleStageAll
  ③ 提交区  flex-shrink-0 border-t border-[var(--border-subtle)] p-2       ← :404-428
     ├ textarea rows=3
     └ 提交按钮 w-full mt-1.5 py-1.5
  ④ DiffModal（heldDiff 非空时）                                           ← :431-439
```

空态/loading 文案样式统一：`text-center text-[var(--text-muted)] text-sm py-6`。

### 4.2 数据分组（`:109-111`）

```ts
staged    = changes.filter(c => c.stagedStatus)
unstaged  = changes.filter(c => c.unstagedStatus && c.unstagedStatus !== 'untracked')
untracked = changes.filter(c => c.unstagedStatus === 'untracked')
```
⚠️ **同一文件可同时出现在 staged 与 unstaged 两组**（部分暂存），这是正确行为。

**分组头**（`renderGroup`，`:320-345`）：`files.length === 0` 时整组不渲染；
头 `flex items-center justify-between px-2 py-1`，
标题 `text-xs text-[var(--text-muted)] uppercase tracking-wider font-medium`，格式 `「{title} ({count})」`；
右侧 action 按钮 `text-xs text-[var(--text-muted)] hover:text-[var(--text-primary)]`。

### 4.3 文件行（`renderFileRow`，`:225-279`）

```
group flex items-center justify-between py-1 px-2
hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] cursor-pointer text-sm
style={{ paddingLeft: `${depth * 16 + 8}px` }}
```
- 左侧状态字符：`shrink-0 text-xs font-mono w-4 text-center` + 颜色（见下表）；
  字符由 `statusLabelFor`（`:60-70`）给：`modified→M · added→A · deleted→D · renamed→R · untracked→? · conflicted→C · 其它→空格`。
  staged 组取 `stagedStatus`，其余取 `unstagedStatus`。
- 文件名 `truncate`，`title = file.path`（完整路径）；
- 右侧行内按钮（`:266-276`）：`w-5 h-5 text-sm opacity-0 group-hover:opacity-100 transition-opacity`，
  staged 显示 `−`（取消暂存），否则 `+`（暂存）；点击 `stopPropagation`；
  title/aria = `t('panels.unstage')` / `t('panels.stage')`；
- 整行点击 → `handleViewDiff(path, isStaged, statusChar)`。

**状态色**（`statusColor`，`:72-82`，按所在区取对应 status）：

| status | class |
|---|---|
| modified | `text-[var(--color-warning,#e5c07b)]` |
| added | `text-[var(--color-success,#98c379)]` |
| deleted | `text-[var(--color-error,#e06c75)]` |
| renamed | `text-[var(--color-info,#61afef)]` |
| untracked | `text-[var(--color-success,#98c379)]` |
| 其它 | `text-[var(--text-muted)]` |

（`conflicted` 落到 default 的 muted——原版如此，照抄。）

### 4.4 树形视图（`buildFileTree` / `renderTreeNode`，`:36-58`、`:281-310`）

- `viewMode` 取自 **config**：`config.gitChangesViewMode ?? 'list'`（`:93`），
  切换写回整份 config（`toggleViewMode`，`:218-221`）。
  Rust 侧字段已在：`crates/mt-config/src/config.rs:134-135` `git_changes_view_mode: String`（默认见 `:434`），
  **是 String 不是枚举**——照 `locale` 的做法解析，认不出的值回落 `"list"`。
- `buildFileTree` 按 `/` 切 path 建目录树；目录节点靠 `!n.file` 与文件区分；
  **不做单链目录压缩**（与 FileTree 的 `compactDirChains` 不同，这里没有）。
- 目录行：`flex items-center gap-1 py-0.5 px-2 text-sm text-[var(--text-muted)] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)]`，
  `paddingLeft = depth*16 + 8`；chevron `▾` `text-sm w-3 text-center`，
  `transform: rotate(-90deg)`（折叠）/`0deg`，`transition: transform 150ms`（**行内写死 150ms**，不是 CSS 变量）。
- 折叠集合 key：`` `${area}:${node.fullPath}` ``（`:285`、`:292`）——**同一路径在三个区各自独立折叠**。
- 折叠态是组件 state（`collapsedDirs: Set<string>`，`:106`），**不落盘**，抽屉一关就没。

### 4.5 交互动作（全部 `await` 后 `loadChanges()` 重取）

| 动作 | 后端 | 行号 |
|---|---|---|
| 暂存单文件 | `git_stage(repoPath, [path])` | `:149-156` |
| 取消暂存单文件 | `git_unstage(repoPath, [path])` | `:158-165` |
| 全部暂存 | `git_stage_all(repoPath)` | `:167-174` |
| 全部取消暂存 | `git_unstage_all(repoPath)` | `:176-183` |
| 提交 | `git_commit(repoPath, msg.trim())` | `:185-198` |
| 丢弃 | `git_discard_file(repoPath, files)` | `:200-212` |

- **失败一律 `console.error` 静默**（不弹 toast、不显红）——原版行为，GPUI 侧对应 `eprintln!`；
  若要提升成 `prompt::show_alert` 属**行为增强**，需在 PR 里说明。
- **丢弃前必须确认**（`:201-205`）：`ask()` 对话框，
  message = `t('gitChanges.discardConfirm', {count})`（含 `\n此操作不可撤销。`），
  title = `t('gitChanges.discardTitle')`，kind=`warning`，
  ok = `t('gitChanges.discardOk')`，cancel = `t('gitChanges.discardCancel')`。
  GPUI 侧走 `crate::prompt::Confirm::new(...).ok_text(..).cancel_text(..).open(..)`（`crates/mt-app/src/prompt.rs:144-211`）。
- **提交**（`:185-198`）：前置 `commitMsg.trim() && staged.length > 0`；
  置 `committing=true` → `git_commit` → 成功清空输入框 + `loadChanges()` + `onCommitSuccess()`；
  finally 复位 `committing`。

### 4.6 提交区（`:404-428`）

- textarea：`w-full text-sm bg-[var(--bg-base)] text-[var(--text-primary)] border border-[var(--border-default)] rounded px-2 py-1.5 resize-none placeholder:text-[var(--text-muted)] select-text`，
  `rows={3}`，placeholder = `t('panels.commitPlaceholder')`；
- **Ctrl+Enter / Cmd+Enter 直接提交**（`:411-415`）；
- 提交按钮：`w-full mt-1.5 py-1.5 text-sm rounded font-medium`；
  可用态 `bg-[var(--accent)] text-white hover:opacity-90 cursor-pointer`，
  禁用态 `bg-[var(--bg-elevated)] text-[var(--text-muted)] cursor-not-allowed`；
  文案 `committing ? t('gitChanges.committing') : t('panels.commit', {count: staged.length})`。

### 4.7 文件行右键菜单（`:242-256`）

顺序（`sep` = 分隔符）：
1. `t('gitChanges.contextViewDiff')` → 查看 diff
2. `sep`
3. staged 区 → `t('panels.unstage')`；其余 → `t('panels.stage')`
4. **仅非 staged 区**：`sep` + `t('gitChanges.contextDiscard')` → 丢弃

### 4.8 自动刷新：pty-output 关键词嗅探

`GitChanges.tsx:19-25` 与 `GitHistoryContent.tsx:188-194` **各有一份完全相同**的正则表：

```ts
const GIT_REFRESH_PATTERNS = [
  /create mode/, /Switched to/, /Already up to date/,
  /insertions?\(\+\)/, /deletions?\(-\)/,
];
```

监听 `pty-output` 事件（`GitChanges.tsx:134-145`）：
1. `if (isAiPty(payload.ptyId)) return;` —— **AI pane 的输出不嗅探**（`src/utils/terminalCache.ts:113-115`，`aiPtyIds` 集合）；
2. 命中任一正则 → `debouncedRefresh()`：**500ms 去抖**（`:128-132`）。

### 4.9 ⚠️ GPUI 侧最大的结构性缺口：没有 pty-output 观察点

原版靠 Tauri 事件总线，任何组件都能订阅 `pty-output` 拿到**原始字节/文本**。
GPUI 侧 reader 线程的回调（`crates/mt-app/src/pane.rs:146-153`）只做三件事：
`emulator.advance(bytes)` → `ai.perception().observe_output(pty_id, bytes)` → 发一个**无载荷**的 `PaneSignal::Output`。
**字节不往上传**，Git 面板拿不到内容去嗅探关键词。

三条可选路（实现者择一，在 PR 里说明选择理由）：

- **(a) 加一条全局输出旁路**：仿 `ai.perception().observe_output` 再挂一个轻量 sink（`thread_local` 或 `Arc<dyn Fn(u32,&[u8])>`），
  Git 面板注册进去。**优点**：语义与原版一字不差（含 `isAiPty` 过滤与 500ms 去抖）。
  **缺点**：热路径上多一次遍历——正则要在**已过滤**的路径上跑，别在 reader 线程直接跑 5 条正则（刷屏时会拖垮吞吐）。
  建议做法：reader 线程只做「有没有 `\n`」这类零成本判断，把最近一段丢进有界环形缓冲，主线程 16ms 节拍上再跑正则。
- **(b) 换成 `.git` 目录监听**：`mt_project::watch::FsWatcher`（`watch.rs:33-65`）监听 `<repo>/.git`，
  `HEAD`/`index`/`refs` 变化即刷新。**优点**：不碰热路径，且覆盖「在别的终端外部跑 git」的场景（原版覆盖不了）。
  **缺点**：与原版行为不同（会多刷、也会漏掉纯工作区改动）。
- **(c) 本批先只做手动刷新按钮 + 操作后自刷**，把自动嗅探留档转下一批。

**推荐 (a)**，理由：`docs/gpui-parity-audit.md` 是"对齐原版"的任务书，行为差异要有明确授权。
若选 (b)/(c)，必须在 `docs/gpui-migration-progress.md` 技术债段留一行。

### 4.10 fs-change 联动：**Git 面板不订阅**

grep 证实：`GitChanges.tsx` / `GitHistory.tsx` / `GitHistoryContent.tsx` **零处** `fs-change`。
唯一订阅 `fs-change` 的是 `src/components/FileTree.tsx:177-178`，而且它只重列目录、**不重取 git 状态**
（`get_git_status` 只在项目切换时跑一次，`FileTree.tsx:496`、`:607`）。
所以「改文件后 Git 面板不自动更新」在原版就是既有行为，本批**不要顺手加**。

---

## 5. 组件 D：GitHistoryContent（「提交历史」区）

源：`src/components/GitHistoryContent.tsx:211-399`。
Props（`:196-205`）：`{ repoPath: string（空串 = 无仓库）, branches: BranchInfo[], viewBranch?: string, refreshRepos: () => void }`。

### 5.1 布局

```
<div h-full bg-[var(--bg-surface)] flex flex-col>
  <div flex-1 overflow-y-auto px-1 py-1  ref=scrollRef onScroll=handleScroll>
     ├ !repoPath → 居中 t('gitHistoryContent.noRepos')   text-sm py-6
     ├ commits.length>0 → RepoCommitList
     ├ loading → t('gitHistoryContent.loading')          text-xs py-2
     └ !loading && commits.length===0 → t('gitHistoryContent.noCommits')  text-xs py-2
  </div>
  CommitDiffModal（heldDiff 非空时）
</div>
```

### 5.2 分页与请求作废（`:234-290`）

- 每页 `limit: 30`（`:244`）；
- 首页：`branch: viewBranch ?? null`；**续页：`branch: null`**（`:246` 原注释：分页从上一页末尾 commit 的 parent 续走，不需要 branch）；
- 续页游标 = 上一页最后一个 commit 的 `hash`（`get_git_log` 内部 push 它的 **parent_ids**，`git.rs:515-520`）；
- **去重是必需的**（`:253-259` 原注释）：有分支时续页会带回已加载的 commit，重复 hash 会让**拓扑图连线算错**。
  按 hash 去重后：`hasMore = page.length >= 30 && merged.length > 之前长度` ——
  整页都是重复的就停止分页，避免用同一个游标反复请求；
- **请求令牌** `reqIdRef`（`:232`）：换仓库/换分支时 `reqIdRef.current++`，迟到响应 `if (id !== reqIdRef.current) return` 丢弃；
- 触底判定（`:283-290`）：`scrollTop + clientHeight >= scrollHeight - 50`；
- `loadingRef` 是**同步**的重入锁（React state 更新是异步的，`:230`）。

### 5.3 提交行（`CommitItem`，`:80-143`）

行高**固定** `GRAPH_ROW_HEIGHT = 48px`（`gitGraph.ts:17`，原注释：连线要跨行接续，行高必须固定）。

```
flex items-stretch cursor-pointer hover:bg-[var(--border-subtle)]
rounded-[var(--radius-sm)] transition-colors duration-100 px-2
style={{ height: 48px }}
title = commit.body ? `${message}\n\n${body}` : message
```

左：`CommitGraphCell`（SVG，宽度 `graph.width`，`shrink-0 pointer-events-none`）
右：`flex-1 min-w-0 flex flex-col justify-center pl-1`
  - 第一行 `text-sm text-[var(--text-primary)] flex items-center gap-1 min-w-0`：
    分支胶囊（若干）+ `<span className="truncate">{message}</span>`
  - 第二行 `text-xs text-[var(--text-muted)] flex items-center gap-1.5 mt-0.5`：
    `author`（`truncate max-w-[140px]`）· `·` · 相对时间（`shrink-0`）· `·` · `shortHash`（`font-mono shrink-0`）

**分支胶囊**（`:110-130`）：`inline-flex items-center shrink-0 text-sm leading-[18px] px-1.5 rounded font-medium`

| 条件 | background | color |
|---|---|---|
| `isHead` | `var(--color-accent, #58a6ff)` | `#fff` |
| `isRemote` | `var(--border-subtle, #3d3d3d)` | `var(--text-muted)` |
| 本地非 HEAD | `rgba(63, 185, 80, 0.2)` | `rgb(63, 185, 80)` |

title 三选一：`gitHistoryContent.remoteBranch` / `.currentBranch` / `.localBranch`（都带 `{name}`）。

⚠️ **只标注本工作区的分支**（`RepoCommitList` `:166-169`，原注释）：
`shownBranches = allBranches.filter(b => b.isHead || b.name === viewBranch)`。
worktree 与主仓库共享 refs，标出全部分支会把其他工作区/远程的分支全挂到 commit 上，看起来像本工作区持有它们。

**相对时间**（`src/utils/timeFormat.ts:3-18`）：`<60s → t('time.justNow')`；`<3600 → t('time.minutesAgo',{n})`；
`<86400 → t('time.hoursAgo',{n})`；`<2592000（30 天）→ t('time.daysAgo',{n})`；再往前 `YYYY-MM-DD`。
⚠️ 命名空间是 **`time`**（`crates/mt-i18n/src/dict.rs:1851` 已在），
**不是** `session_panel.rs:83-101` 用的 `sessionList.time.*`——两套 key 并存，别串。
且原版 30 天以上是**纯数字日期**，没有 `time.monthDay` 这种 key。

### 5.4 拓扑图（`src/utils/gitGraph.ts`，213 行，**必须逐条移植**）

常量（`:15-19`）：`GRAPH_LANE_WIDTH = 14` · `GRAPH_ROW_HEIGHT = 48` · `GRAPH_MAX_LANES = 8`
调色板（`:21-30`，8 色循环）：`#58a6ff #3fb950 #d29922 #bc8cff #f78166 #39c5cf #db61a2 #a5d6ff`

**算法**（`computeGitGraph`，`:61-141`）：自上而下扫描，维护 `lanes: ({hash, color} | null)[]`：
1. 找出所有 `lanes[i].hash === commit.hash` 的 lane（`incoming`）；
2. 节点落在**最左**的 incoming lane，颜色继承；没有 incoming 则 `allocLane()` 新开 + `nextColor()`（分支尖端）；
3. 与本 commit 无关的 lane 画**直穿段** `{from:i, to:i}`；
4. 上半程：每条 incoming 画 `{from:i, to:-1, color: lanes[i].color, endColor: 节点色}`；除节点 lane 外全部释放；
5. 下半程：先释放自己那条 lane，再派发 parents：
   - 若某 parent **已有 lane 在等它** → 画 `{from:-1, to:existing, color: 节点色, endColor: 目标 lane 色}`，
     **不另开 lane**（`:118-124` 原注释：线的颜色跟着分支走，全程保持自己的颜色，只在根部渐变融入主线）；
   - 否则 `pi===0` 继承节点 lane 与颜色，`pi>0` 各 `allocLane()` + `nextColor()`；
6. `isMerge = parents.length >= 2`；
7. `width = min(maxLane+1, 8) * 14 + 4`。

**路径编译**（`segmentPath`，`:158-187`）：`CURVE = GRAPH_ROW_HEIGHT / 4 = 12`（`:155`）
- 直穿同 lane：`M x 0 V 48`；直穿异 lane：`M xf 0 C xf 24 xt 24 xt 48`
- 上半程同 lane：`M xf 0 V 24`；异 lane：`M xf 0 C xf 12 xn 12 xn 24`
- 下半程同 lane：`M xn 24 V 48`；异 lane：`M xn 24 C xn 36 xt 36 xt 48`
- `laneX(lane) = min(lane, 7) * 14 + 7`（`:144-147`）

**渐变**（`:190-212`）：`needsGradient = !!endColor && endColor !== color`；
`<linearGradient gradientUnits="userSpaceOnUse">` 三个 stop：`0% color` / `70% color` / `100% endColor`（`GitHistoryContent.tsx:52-54`）。
⚠️ `useId()` 带冒号，SVG 的 `url(#…)` 引用里去掉（`GitHistoryContent.tsx:33`）——GPUI 侧无此问题。

**节点圆**（`GitHistoryContent.tsx:68-75`）：
- merge：空心 `r=5.5 fill=none stroke=color strokeWidth=1.5 opacity=0.55` + 实心 `r=3 fill=color`
- 普通：实心 `r=4 fill=color`
- 线段 `strokeWidth=1.5 fill=none`

**GPUI 渲染路线**：gpui 的 `svg()` 是单色 alpha 掩膜（丢色），Image 的 SVG 分支有 BGRA 交换 bug ——
两条都不能用（`docs/gpui-parity-audit.md:34` K 批记档）。走 `mt_ui` 的 `PathBuilder` 自绘 DSL：
贝塞尔用 `curve_to`，渐变用**分段近似**（把 0%→70%→100% 拆成若干等色小段）或直接退化成"根部一小段用 endColor"。
渐变是纯装饰，退化可接受，但要在注释里写清取舍。

### 5.5 交互

- **双击行** → `handleViewDiff(repoPath, commit)`（`:105`）：先 `get_commit_files` 拿文件列表，再开 `CommitDiffModal`；
  失败 `console.error` 静默（`:305`）。⚠️ **是双击不是单击**。
- **右键行**（`:310-326`）：
  1. `t('gitHistoryContent.copyCommitHash')` → 写剪贴板 **完整 hash**（不是 shortHash）
  2. `separator`
  3. `t('gitHistoryContent.viewChanges')` → 同双击
- **pty-output 嗅探**（`:338-349`）：同 §4.8，去抖回调里做两件事：`refreshRepos()` + `load()`（`:330-336`）。

---

## 6. 组件 E：DiffModal（工作区/暂存区单文件 diff）

源：`src/components/DiffModal.tsx`。**导出三个东西**：`InlineView`（`:22-58`）、`SideBySideView`（`:62-152`）、`DiffModal`（`:156-262`）。
前两个被 `CommitDiffModal` 复用（`CommitDiffModal.tsx:3`）——GPUI 侧同样拆成公共渲染函数。

### 6.1 入口（两处）

1. GitChanges 的文件行点击/右键（`GitChanges.tsx:431-439`）：
   `projectPath={repoPath}`，`status={{path, status:'modified', statusLabel: 状态字符}}`，`staged={heldDiff.staged}`；
2. FileTree 右键「查看变更」（`FileTree.tsx:321`、`:850`）——**GPUI 侧 `file_tree.rs:291`/`:777` 现在刻意跳过该项，本批要补回**。

### 6.2 弹窗外壳

`<Modal open onClose align="center" ariaLabel={fileName} panelClassName="w-[90vw] h-[80vh] select-text">`（`:187-188`）
`useOverlayPresence(open)` 短路（`:163`、`:182`），退场动画期间不塌子树。
GPUI 侧走 `crate::prompt::open_guarded(kind, ...)`（`crates/mt-app/src/prompt.rs:48`）防叠开，
需在 `crates/mt-app/src/overlay.rs` 的 `pub mod kind`（`:51-71`）里加常量，建议：
`GIT_DIFF = "git-diff"` · `GIT_COMMIT_DIFF = "git-commit-diff"` · `GIT_WORKTREE = "git-worktree"` ·
`GIT_WORKTREE_REMOVE = "git-worktree-remove"`（嵌套确认要能与外层并存 → **不同 kind 可叠**，见 N 批设计）。

### 6.3 工具栏（`:190-230`）

`flex items-center justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0`
- 左：`fileName`（`text-base font-medium text-[var(--accent)]`）+ 完整 path（`text-sm text-[var(--text-muted)] truncate max-w-[300px]`）
  + 状态胶囊（`px-2 py-0.5 text-xs rounded bg-[var(--bg-elevated)] text-[var(--text-muted)] border border-[var(--border-subtle)]`，内容 `status.statusLabel`）
- 右：视图段控件（`flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden`，
  两个 `px-3 py-1 text-sm` 按钮；选中 `bg-[var(--accent-subtle)] text-[var(--accent)]`，未选 `text-[var(--text-muted)] hover:text-[var(--text-primary)]`）
  + `✕` 关闭（`text-lg leading-none ml-2`）
- `viewMode` 默认 `'side-by-side'`（`:158`），**组件 state，不落盘**。

### 6.4 内容区状态机（`:233-259`）

`flex-1 overflow-auto bg-[var(--bg-base)]`，五选一（互斥，按顺序判）：

1. `loading` → 居中 `t('diffModal.loading')`，`text-[var(--text-muted)]`
2. `error`（非空串）→ 居中 **错误原文**，`text-[var(--color-error)]`
3. `diffResult.isBinary` → 居中 `t('diffModal.binaryNotSupported')`
4. `diffResult.tooLarge` → 居中 `t('diffModal.tooLarge')`
5. 否则 → `SideBySideView` 或 `InlineView`

取数（`:165-179`）：`open` 变 true 时置 loading/清 error/清结果 → `get_git_diff({projectPath, filePath: status.path, staged: staged ?? false})`。

⚠️ **已知边界（原版 bug，照抄或顺修需说明）**：effect 依赖数组是 `[open, projectPath, status.path]`（`:179`），
**漏了 `staged`**。同一路径先点 staged 行再点 unstaged 行（组件未卸载，`heldDiff` 一直非空），
diff 内容不会重新拉取。GPUI 侧若按 `(repo, path, staged)` 三元组做 key 就自然修掉，建议**顺修并在 PR 里注明**。

### 6.5 InlineView（`:22-58`）

```
<div className="font-mono" style={{ fontSize: `${fontSize}px`, lineHeight: `${Math.round(fontSize*1.6)}px` }}>
```
`fontSize` 来自 `config.terminalFontSize || 14`（`:162`；Rust 侧 `store.config().terminal_font_size`，`store.rs:890` 已有取用）。
每个 hunk 一个 wrapper，每行：
```
flex + (add ? bg-[var(--diff-add-bg)] : delete ? bg-[var(--diff-del-bg)] : '')
  ├ 行号列  w-[48px] text-right pr-2 text-[var(--text-muted)] select-none flex-shrink-0 opacity-50
  │        内容：add→'+'  delete→'-'  context→oldLineno（可能为空）
  └ 内容列  flex-1 whitespace-pre px-2
           色：add→var(--diff-add-text) / delete→var(--diff-del-text) / context→var(--text-primary)
```
⚠️ hunk 之间**没有 `@@ -a,b +c,d @@` 头**——原版不画，别自作主张加。

### 6.6 SideBySideView（`:62-152`）

**配对算法**（`:63-97`，纯逻辑，必须逐条移植）：
```
for hunk in hunks:
  i = 0
  while i < lines.len:
    kind == context           → rows.push({left: line, right: line}); i++
    kind == delete            → 连续吃掉所有 delete 到 deletes[]，再连续吃掉紧随的 add 到 adds[]
                                for j in 0..max(deletes.len, adds.len):
                                  rows.push({left: deletes[j], right: adds[j]})   // 短的一侧为 undefined
    kind == add               → rows.push({left: undefined, right: line}); i++
    其它                       → i++
```
**空格子**（`:100-107`）：`flex h-full bg-[var(--bg-base)] opacity-30`，内部 `w-[48px]` 占位 + `flex-1` 空。
**有内容格子**：与 InlineView 同构，但行号列**左侧显示 `oldLineno`、右侧显示 `newLineno`**（`:117`）。

**两栏容器**用 `Allotment`（可拖拽），两个 `Allotment.Pane` 各自 `h-full overflow-auto`（`:134-149`）。
⚠️ **两栏滚动不同步**（原版就没同步）——GPUI 侧用 `h_resizable` 对齐即可，别顺手加同步。

**GPUI 性能坑**：大文件 diff 可能上万行。原版靠浏览器的 DOM 虚拟化…… 其实**没有虚拟化**，
`rows.map` 全量渲染（`:137`、`:144`）。1MB 上限（`MAX_DIFF_BYTES`）挡住了最坏情况，但一个 900KB 的文本文件仍能出 ~20k 行。
GPUI 全量建元素会明显卡。**建议用 `gpui::uniform_list`**（行高恒定 = `round(fontSize*1.6)`，天然适配），
并在注释里写明这是相对原版的**改进**而非偏差。

---

## 7. 组件 F：CommitDiffModal（某次 commit 的多文件 diff）

源：`src/components/CommitDiffModal.tsx`。
Props：`{ open, onClose, repoPath, commitHash, commitMessage, files: CommitFileInfo[] }`。

### 7.1 布局：左右两栏（`panelClassName="w-[92vw] h-[85vh] select-text flex-row"`，`:88`）

**左栏**（`:90-124`）：`w-56`（224px）`flex-shrink-0 border-r border-[var(--border-subtle)] flex flex-col bg-[var(--bg-elevated)]`
- 头部 `px-3 py-3 border-b`：commit message（`text-sm font-medium text-[var(--accent)] truncate`）
  + 短 hash（`text-xs text-[var(--text-muted)] mt-1 font-mono`，`commitHash.slice(0,7)`，`:84`）
- 文件列表 `flex-1 overflow-y-auto`，每行 `flex items-center gap-2 px-3 py-1.5 cursor-pointer text-sm`：
  状态字母（`text-xs font-bold flex-shrink-0`）+ **文件名**（`path.split('/').pop()`，`truncate`），`title = 完整 path`；
  选中 `bg-[var(--accent-subtle)] text-[var(--accent)]`，否则 `text-[var(--text-primary)] hover:bg-[var(--border-subtle)]`
- 底部 `px-3 py-2 border-t text-xs text-[var(--text-muted)]`：`t('commitDiff.fileCount', {count: files.length})`

**状态字母表**（`STATUS_LABELS`，`:21-26`）：
`added → A / text-[var(--color-success)]` · `modified → M / warning` · `deleted → D / error` · `renamed → R / info`；
**查不到（如 conflicted/untracked）回落 `?` + muted**（`:99`）。

**右栏**（`:127-198`）：`flex-1 flex flex-col`
- 工具栏 `flex items-center justify-between px-4 py-3 border-b flex-shrink-0`：
  左 = 当前文件**完整路径**（`text-sm text-[var(--text-primary)] truncate max-w-[400px]`）；
  右 = 视图段控件（同 DiffModal，文案 `t('commitDiff.sideBySide')`/`t('commitDiff.inline')`）+ `✕`
- 内容区 `flex-1 overflow-auto bg-[var(--bg-base)]`，**六选一**：
  1. `loading` → `t('commitDiff.loading')`
  2. `error` → 错误原文，`text-[var(--color-error)]`
  3. `isBinary` → `t('commitDiff.binaryFile')`
  4. `tooLarge` → `t('commitDiff.tooLarge')`
  5. 正常 → `SideBySideView` / `InlineView`
  6. `!loading && !error && !diffResult && files.length === 0` → `t('commitDiff.noChanges')`

### 7.2 数据流

- `selectedFile` 初值 `files[0]?.path ?? ''`（`:38`）；
- effect A（`:69-73`）：`open && selectedFile` 变化 → `loadDiff(selectedFile)`；
- effect B（`:75-79`）：`open && files.length>0 && 当前选中不在 files 里` → 自动选回 `files[0].path`；
- `loadDiff`（`:45-67`）：置 loading/清 error/清结果 → `get_commit_file_diff({repoPath, commitHash, filePath, oldFilePath: fileInfo?.oldPath ?? null})`。
  **重命名文件必须传 `oldPath`**，否则父树里查不到旧内容，diff 会显示成"整文件新增"。

---

## 8. 组件 G：GitWorktreeModal（833 行，本批最大件）

源：`src/components/GitWorktreeModal.tsx`。
Props（`:16-29`）：`{ repoPath: string|null（null=关闭）, discoverRepos?: boolean, onChanged: ()=>void, onClose: ()=>void, projectId?: string }`

### 8.1 两个入口（语义不同）

| 入口 | 源 | `discoverRepos` | `projectId` | `onChanged` |
|---|---|---|---|---|
| Git 面板仓库栏右键「Worktree 管理」 | `GitHistory.tsx:541-545` | **不传**（单仓库模式，`repoPath` 就是仓库根） | 不传（用 activeProjectId） | `loadRepos`（刷新仓库列表） |
| 项目列表右键「Worktrees」 | `ProjectList.tsx:1129-1135`，菜单项在 `:735-738` | **传 true**（项目根未必是仓库，向下发现） | `worktreeTarget.projectId` | **空函数**（`:1134`，原注释：后端已在增删后失效缓存，Git 抽屉下次加载即为新数据） |

### 8.2 RepoGroup 归并（`:31-42`、`:107-181`）

```ts
interface RepoGroup {
  key: string;        // normalizePath(主仓库路径)
  name: string;       // 主仓库目录名（worktree 目录建议名的前缀）
  mainPath: string;   // git 命令的执行路径：worktree 增删必须落在主仓库上
  worktrees: WorktreeInfo[];
  error?: string;     // list_worktrees 失败（仓库损坏等），该组只展示错误
}
```

加载流程：
1. `discoverRepos` → `discover_git_repos(repoPath)` 拿路径集；否则 `repoPaths = [repoPath]`；
   discover 失败 → `groups=[]` + `loadError`；
2. 对每个路径并发 `list_worktrees`，失败记 `{worktrees: null, error}`；
3. **单仓库时沿用旧行为**（`:138-143`）：加载失败即整体报错（`loadError`），不显示空壳分组；
4. **按主工作区归并**（`:145-172` 原注释）：扫描可能同时发现主仓库与它在项目目录内的 worktree，
   两者的 `list_worktrees` 结果**完全相同**，合成一组才不会重复展示。
   归并键 = `normalizePath(worktrees.find(isMain)?.path ?? item.path)`；已有该键则 `continue`；
5. 勾选保留（`:175-180`）：重新加载时保留原有勾选（剔除消失的键）；**只剩一个仓库且无 error 时自动勾选**。

`normalizePath`（`src/utils/projectActions.ts:9-11`）：`p.replace(/[\\/]+/g,'/').replace(/\/$/,'').toLowerCase()`
—— **必须逐字移植**，worktree「是否已是项目」的比对全靠它。

### 8.3 布局（`:555-831`）

```
<Modal open={!!repoPath} title={t('worktree.title', {name: rootName})} panelClassName="w-[600px] max-h-[85vh]">
  <div className="flex-1 min-h-0 overflow-y-auto p-4 select-none">
    ── 工作区列表 ──
    ├ groups===null      → t('worktree.loading')          text-sm text-muted py-4 text-center
    ├ loadError          → 错误原文  text-sm text-[var(--color-error)] py-2 break-all
    ├ groups.length===0  → t('worktree.noRepoFound')      同 loading 样式
    └ 否则 space-y-2：
       ├ multiRepo 时：头行「t('worktree.reposFound',{count})」+ 全选/取消全选按钮
       └ 每组：
          ├ multiRepo 时组框 rounded-[var(--radius-sm)] border border-[var(--border-subtle)]
          ├ multiRepo 时组头 <label> checkbox + 组名 + mainPath（ml-auto，truncate）
          │   选中 bg-[var(--accent-subtle)] / 组名 text-[var(--accent)]
          ├ g.error → px-2 py-1.5 text-xs text-[var(--color-error)] break-all
          ├ 否则 worktree 行列表（multiRepo ? "px-1 pb-1 space-y-0.5" : "space-y-0.5"）
          └ hasInvalid → 右对齐「清理失效条目」按钮
    ── 新建 worktree（groups 非空时）── mt-4 pt-3 border-t
       ├ 标题行：t('worktree.createTitle') + multiRepo 时 t('worktree.selectedCount') + 模式段控件（ml-auto）
       ├ 模式区：
       │   selectedGroups.length===0 → t('worktree.selectRepoHint')
       │   existing → !branchesReady ? loading
       │               : availableBranches 空 ? (multiTarget ? noCommonBranch : noBranchAvailable)
       │               : <select>（首项 value="" 文案 t('worktree.selectBranch')）
       │   new     → <input 新分支名> + <select 起点分支 w-[180px]>（首项 t('worktree.baseHead')）
       ├ 路径行：<input flex-1> + 「浏览…」按钮
       ├ multiTarget && 有分支名 → 逐条列出将建的目录（text-xs font-mono truncate）
       ├ <label> checkbox「创建后添加为项目并切换过去」
       ├ createError / createResults（逐仓库错误）
       └ 右对齐创建按钮
  </div>
  ── 嵌套删除确认 Modal（w-[400px]）──
</Modal>
```

### 8.4 worktree 行（`renderWorktree`，`:489-553`）

```
group flex items-center gap-2 px-2 py-1.5 rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]/60
```
- 左（`flex-1 min-w-0`）：
  - 第一行：名称（`text-sm font-medium text-[var(--text-primary)] truncate`）+ 徽章序列
  - 第二行：`wt.path`（`text-xs text-[var(--text-muted)] truncate`，`title=path`）
- 徽章顺序与样式：
  - `isMain` → `t('worktree.mainRepo')`，`badgeCls`
  - `branch` → `⎇ {branch}`，`badgeCls`
  - `!isValid` → `t('worktree.invalid')`，`text-xs leading-[16px] px-1.5 rounded font-medium text-[var(--color-error)] bg-[var(--color-error)]/15`
  - `isLocked` → `t('worktree.locked')`，`badgeCls`
  - 已是项目 → `t('worktree.isProject')`，`text-[var(--accent)] bg-[var(--accent-subtle)]`（同上尺寸）
  - `badgeCls`（`:66-67`）= `shrink-0 text-xs leading-[16px] px-1.5 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]`
- 右（**仅 `isValid` 时**，`opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity`）：
  - `t('worktree.openTerminal')` → `newTerminal(targetProjectId, undefined, {cwd: wt.path, title: `⎇ ${wt.branch ?? wt.name}`})` + **关弹窗**（`:421-429`）
  - `isProject ? t('worktree.switchToProject') : t('worktree.addAsProject')` → `switchToProjectAt(...)` + 关弹窗
  - **非 main 才有** `t('worktree.remove')`（危险色 hover：`hover:text-[var(--color-error)] hover:bg-[var(--color-error)]/10`）
  - 普通按钮 `actionBtnCls`（`:486-487`）= `shrink-0 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]`

「已是项目」判定（`:490-492`）：`projects.some(p => !p.sshConnectionId && normalizePath(p.path) === normalizePath(wt.path))`
—— 组件订阅了 `config.projects`（`:79`），增删项目即时反映。

### 8.5 分支交集逻辑（`:239-268`，多仓库批量的核心）

- `branchesByRepo: Map<groupKey, BranchInfo[]>`，**按需惰性拉**（`:213-237`）：只对**已勾选**的组调 `get_repo_branches`；
  失败也落一条空记录，否则 effect 会反复重试（`:222-223` 原注释）；
- `branchesReady` = 勾选组非空 **且** 每个勾选组都有记录；
- **`availableBranches`（检出现有分支）**（`:243-258`）：
  逐组算「本地分支 − 该组已被任一 worktree 占用的分支」，再取**全组交集**，顺序按第一个组的列表；
- **`baseBranchOptions`（新分支起点）**（`:261-268`）：逐组的**全部分支名**（含远程）取交集。

### 8.6 目标路径推导（`:270-316`）

- 分隔符 `sep = repoPath.includes('\\') ? '\\' : '/'`（`:270`）；
- `multiRepo = groups.length > 1`；`multiTarget = selectedGroups.length > 1`（`:208-210`）
  —— **多个仓库被勾选时，路径输入框语义变成「父目录」**；
- `sanitizeBranchForDir`（`:45-47`）：`branch.replace(/[\\/:*?"<>|\s]+/g,'-').replace(/^-+|-+$/g,'') || 'worktree'`；
- `targets`（`:276-285`）：单选 = 输入框原样；多选 = `joinPath(父目录, `${组名}-${sanitize(分支)}`, sep)`；
- **默认路径建议**（`:289-303`）：
  - 未勾选 → 清空；
  - 多选 → `repoPath`（作父目录）；
  - 单选 → `joinPath(parentDir(g.mainPath), `${g.name}-${sanitize(分支)}`, sep)`（**仓库同级**）；
  - `pathEdited` 为 true（用户手改过）后**不再跟随**；
- 「浏览…」（`:305-316`）：目录选择对话框；多选/未选 → 选到啥用啥；
  单选 → 自动拼子目录（worktree 目录本身必须是**新路径**）；选完置 `pathEdited=true`。
  GPUI 侧目录选择器已有先例：`crates/mt-app/src/modal.rs:598` 的 `ghost_button("browse-dir", t("worktree","browse"))`
  —— **`worktree.browse` 这个 key 已经被复用了**，别以为它没接线。

### 8.7 创建（`handleCreate`，`:359-419`）

1. 前置：分支名非空 + `targets` 非空 + 未在创建中；
2. **并发**对每个 target 调 `add_worktree({repoPath: group.mainPath, worktreePath: target.path, branch, createBranch: mode==='new', base: mode==='new' && baseBranch ? baseBranch : null})`，
   逐个 catch 成 `{target, error}`；
3. 无论成败先 `onChanged()` + **清空 `branchesByRepo` 缓存**（分支集合已变，`:384`）；
4. `addAsProject` 勾着且有成功项 → 对每个成功的 target `addProjectAt(...)`，记住第一个新项目 id，`saveConfigToDisk()`；
5. **部分失败** → 留在弹窗里列出逐仓库错误（`createResults`），`await load()` 刷新列表后 return；
6. **全成功 + addAsProject** → `setActiveProject(firstNewProject)` + `onClose()`；
7. **全成功 + 不加项目** → 清空分支输入、`pathEdited=false`、`await load()`；
8. 外层 catch → `createError`；finally 复位 `creating`。

`addProjectAt`（`:341-351`）：
- 路径已是项目 → 返回既有 id（**不重复添加**）；
- 否则 `addProject({id: genId(), name: baseName(path) || fallbackName, path}, parent?.id)`；
- **父项目**：优先 `findProjectByPath(mainPath)`（主仓库对应的项目），否则回落到 `projectId ?? activeProjectId` 那个项目。

⚠️ GPUI 侧 `AppStore::add_project`（`store.rs:234`）签名是 `(&mut self, path: &Path, cx)`，
**不接受 `parent_project_id`**，且内部自己 `gen_id`、自己 push 进 `project_tree`。
`docs/gpui-parity-audit.md:73` 已把「addProject 的 parentProjectId」列为 store 缺失 action。
本批需要扩它（加 `parent: Option<&str>` 参数或另开 `add_child_project`），并让它**返回新项目 id**。

### 8.8 删除（`handleRemove`，`:431-455`）+ 嵌套确认弹窗（`:779-830`）

**确认弹窗**（`w-[400px]`，`closeOnEscape={!removing}`，`:786`）：
- 标题 `t('worktree.removeConfirmTitle')`（`text-sm font-medium mb-2`）
- 正文 `t('worktree.removeConfirmMessage', {name})` + 下方 `wt.path`（`text-[var(--text-muted)] mt-1 font-mono`）
- **若有项目指向该目录** → 警示行 `t('worktree.removeAlsoProject', {name: 项目名})`，`text-xs text-[var(--color-warning,#f59e0b)] mb-2`
- 复选框 `t('worktree.forceRemove')`（`accent-[var(--color-error)]`）
- `removeError` → `text-xs text-[var(--color-error)] break-all whitespace-pre-wrap mb-3`
- 按钮：取消（ghost，`disabled={removing}`）+ 删除（`bg-[var(--color-error)] text-white`，文案 `removing ? t('worktree.removing') : t('worktree.removeConfirm')`）
- 原注释（`:779`）：**嵌套弹窗；Esc 归栈顶，不会误关外层**。GPUI 侧靠焦点链天然成立（`overlay.rs:14-19`），但 kind 要不同才能叠。

**执行顺序（顺序本身是坑，别调换）**（`:435-449` 原注释）：
1. `findProjectByPath(wt.path)` 找到指向该目录的项目；
2. **先 `disposeProjectTerminals(project.id)`**——Windows 下 shell 占着目录会让 `git worktree remove` 失败；
3. `remove_worktree({repoPath: mainPath, worktreePath: wt.path, force: removeForce})`；
4. **成功后**才 `removeProjectWithCleanup(project.id)`（不留断链项目）；失败时项目还在（终端呈断开态，可重开）；
5. 关确认框 + `onChanged()` + `await load()`。

`disposeProjectTerminals`（`projectActions.ts:25-32`）= 对该项目所有 ptyId 调 `kill_pty` + dispose 前端实例；
`removeProjectWithCleanup`（`:38-42`）= dispose + `removeProject(id)` + `saveConfigToDisk()`。
GPUI 侧：`AppStore::remove_project`（`store.rs:340`）已含终端回收，直接用；
「只 dispose 不删项目」需要新增一个方法（本批要补）。

### 8.9 清理失效（`handlePrune`，`:457-481`）

1. 先记下该组 `!isValid` 的路径集；
2. `prune_worktrees(group.mainPath)`；
3. **`filter_directories(invalidPaths)` 复核哪些目录真的不在了**（`:466-468` 原注释：
   以「目录确实已不存在」为准；`isValid=false` 但目录还在（元数据损坏）时**项目保留**）；
4. 对确实消失的路径，`removeProjectWithCleanup`；
5. `onChanged()` + `await load()`；
6. **失败静默**（`:476-478` 原注释：prune 失败无害，下次打开重试即可）。

按钮：`pruningKey === g.key` 时 disabled + 文案 `t('worktree.pruning')`，否则 `t('worktree.prune')`。

### 8.10 表单重置（`:184-202`）

`repoPath` 变化时**全量重置**：`groups=null`、`selectedKeys=[]`、`branchesByRepo=new Map()`、`loadError=null`、
`mode='existing'`、`selBranch/newBranch/baseBranch/wtPath` 清空、`pathEdited=false`、
**`addAsProject=true`**（默认勾选）、`creating=false`、`createError/createResults=null`、`removeTarget=null`，然后 `load()`。

勾选变化（`toggleRepo` `:318-325` / `toggleAll` `:327-335`）还要清 `pathEdited/selBranch/baseBranch/createError/createResults`。

---

## 9. i18n key 全表（逐个列出）

**好消息：本批用到的 key 在 `crates/mt-i18n/src/dict.rs` 里已经全部存在**（TS 侧字典早已生成过），
**不需要**改 TS 源头、不需要重跑 `gen_from_ts.mjs`、不需要动 `crates/mt-i18n/tests/consistency.rs` 的总数常量。

**唯一要改的**：`crates/mt-app/src/i18n.rs:122` 的 `USED_KEYS` 数组——把下列 key **全部加进去**，
且必须**字典序排列且无重复**（`i18n.rs:290-293` 有测试钉死）。

### 9.1 `panels`（`src/i18n/locales/panels.ts`）

`panels.sessions` · `panels.git` · `panels.changes` · `panels.history` ·
`panels.stagedChanges` · `panels.unstagedChanges` · `panels.untrackedFiles` ·
`panels.commitPlaceholder` · `panels.commit`（`{count}`） · `panels.stage` · `panels.unstage`

### 9.2 `gitHistory`（3 条，全用）

`gitHistory.selectProject` · `gitHistory.selectRepo` · `gitHistory.remoteNotSupported`

### 9.3 `gitHistoryContent`（14 条，全用）

`copyCommitHash` · `currentBranch`（`{name}`） · `loading` · `localBranch`（`{name}`） · `manageWorktrees` ·
`noCommits` · `noRepos` · `openInTerminal` · `refresh` · `remoteBranch`（`{name}`） ·
`switchBranchHint` · `viewChanges` · `viewingBranchHistory`（`{branch}` `{head}`） · `worktreeBadgeTitle`

### 9.4 `gitChanges`（14 条，全用）

`committing` · `contextDiscard` · `contextViewDiff` · `discardCancel` · `discardConfirm`（`{count}`） ·
`discardOk` · `discardTitle` · `empty` · `loading` · `refresh` · `stageAll` · `switchToList` · `switchToTree` · `unstageAll`

### 9.5 `commitDiff`（7 条，全用）

`binaryFile` · `fileCount`（`{count}`） · `inline` · `loading` · `noChanges` · `sideBySide` · `tooLarge`

### 9.6 `diffModal`（5 条，全用）

`binaryNotSupported` · `inline` · `loading` · `sideBySide` · `tooLarge`

### 9.7 `worktree`（42 条，全用）

`addAsProject` · `addAsProjectAfterCreate` · `baseBranchTitle` · `baseHead` · `browse`（**已被 modal.rs:598 用**） ·
`cancel` · `clearAll` · `create` · `createMulti`（`{count}`） · `createTitle` · `creating` · `forceRemove` ·
`invalid` · `isProject` · `loading` · `locked` · `mainRepo` · `modeExisting` · `modeNew` ·
`newBranchPlaceholder` · `noBranchAvailable` · `noCommonBranch` · `noRepoFound` · `openTerminal` ·
`parentPathPlaceholder` · `pathPlaceholder` · `prune` · `pruning` · `remove` · `removeAlsoProject`（`{name}`） ·
`removeConfirm` · `removeConfirmMessage`（`{name}`） · `removeConfirmTitle` · `removing` ·
`reposFound`（`{count}`） · `selectAll` · `selectBranch` · `selectRepoHint` · `selectedCount`（`{count}`） ·
`switchToProject` · `title`（`{name}`）

### 9.8 `time`（4 条，提交行相对时间）

`time.justNow` · `time.minutesAgo`（`{n}`） · `time.hoursAgo`（`{n}`） · `time.daysAgo`（`{n}`）

### 9.9 `app.activityBar`（抽屉/边条）

`app.activityBar.git` · `app.activityBar.closeDrawer`（`app.activityBar.sessions` 已在 `USED_KEYS`）

### 9.10 **无 i18n 的硬编码串**（照抄，别顺手翻译）

- `'Git Pull'` / `'Git Push'`（`GitHistory.tsx:57` 的 title）
- 状态字母 `M A D R ? C`、`⎇`、`▾`、`↻ ↓ ↑ ✓ ✕ ⊞ ≡ − +`
- 分组头的括号计数格式 `「{title} ({count})」`

---

## 10. 样式要点汇总（`src/styles.css` 关键值）

### 10.1 需要新增到 `crates/mt-app/src/ui.rs::Palette` 的 token

现有 `Palette`（`ui.rs:16-38`）**缺 4 个 diff 色 + 1 个 border_strong**：

| CSS 变量 | dark（`styles.css:40-43`） | light（`:114-117`） | blueprint（`:994-997`） | fluent2（`:1085-1088`） |
|---|---|---|---|---|
| `--diff-add-bg` | `rgba(60,180,60,0.12)` | `rgba(40,140,40,0.10)` | `rgba(34,197,94,0.12)` | `rgba(108,203,95,0.16)` |
| `--diff-del-bg` | `rgba(220,60,60,0.12)` | `rgba(200,50,40,0.10)` | `rgba(239,68,68,0.12)` | `rgba(255,120,120,0.16)` |
| `--diff-add-text` | `#6bb87a` | `#2d8a46` | `#22c55e` | `#6ccb5f` |
| `--diff-del-text` | `#d4605a` | `#c0392b` | `#ef4444` | `#ff7878` |
| `--border-strong` | `rgba(255,255,255,0.12)`（`:29`） | `rgba(0,0,0,0.15)`（`:103`） | `rgba(96,165,250,0.25)` | `rgba(255,255,255,0.16)` |

⚠️ `mt_ui::theme_bridge::ThemeSlot`（`crates/mt-ui/src/theme_bridge.rs:464-485`）**没有 diff 槽位**。
主题包（`from_pack`，`ui.rs:147`）里这 4 个色**没有来源**——按 `success`/`error` 派生（`with_alpha(color_success(), 0.12)` 等）
或直接沿用 dark/light 基线。**别为此扩 ThemeSlot**（会改动主题包格式，超出本批范围），在注释里写明取舍。

### 10.2 尺寸速查

| 元素 | 值 | 源 |
|---|---|---|
| 抽屉宽度范围 / 默认 | 240 ~ 720 / **340** | `RightDrawer.tsx:8-9`、`App.tsx:541` |
| 抽屉 z-index | 45（分隔条 35、弹窗 50） | `RightDrawer.tsx:65-66` |
| 抽屉标题条高 | `h-9` = 36px | `RightDrawer.tsx:80` |
| 抽屉左缘手柄 | `w-1.5` = 6px，`-translate-x-1/2` | `RightDrawer.tsx:75` |
| 仓库栏高 | `h-[34px]` | `GitHistory.tsx:354` |
| SectionHeader 高 | `h-[30px]` | `GitHistory.tsx:83` |
| 中缝手柄 | `h-1.5`（6px），`-top-[3px]`，包裹 `h-0 z-30` | `GitHistory.tsx:517-519` |
| 区块比例 clamp | 0.15 ~ 0.85，初值 0.5 | `GitHistory.tsx:15-17` |
| 提交行高 | **48px 固定** | `gitGraph.ts:17` |
| lane 宽 / 最大 lane 数 | 14px / 8 | `gitGraph.ts:15,19` |
| 拓扑图宽 | `min(maxLane+1, 8) * 14 + 4` | `gitGraph.ts:139-140` |
| 贝塞尔控制点偏移 | `48/4 = 12` | `gitGraph.ts:155` |
| diff 行号列宽 | `w-[48px]` | `DiffModal.tsx:38,116` |
| diff 行高 | `round(fontSize * 1.6)` | `DiffModal.tsx:24,130` |
| diff 字号 | `config.terminalFontSize \|\| 14` | `DiffModal.tsx:162`、`CommitDiffModal.tsx:39` |
| DiffModal 面板 | `w-[90vw] h-[80vh]` | `DiffModal.tsx:188` |
| CommitDiffModal 面板 / 左栏 | `w-[92vw] h-[85vh]` / `w-56`(224px) | `CommitDiffModal.tsx:88,90` |
| GitWorktreeModal 面板 / 确认框 | `w-[600px] max-h-[85vh]` / `w-[400px]` | `GitWorktreeModal.tsx:560,785` |
| 文件行缩进 | `depth * 16 + 8` px | `GitChanges.tsx:240,290` |
| 分支名截断 | `max-w-[140px]` | `GitHistory.tsx:426` |
| 分支下拉 | `min-w-[180px] max-h-[320px]`，`zIndex: 100` | `GitHistory.tsx:431-432` |
| 退场驻留 | 400ms（`OVERLAY_EXIT_MS`） | `useOverlayMotion.ts:19` |

### 10.3 硬编码色值（不走 CSS 变量，照抄）

- 本地分支绿：`rgb(63, 185, 80)`；本地分支胶囊底 `rgba(63, 185, 80, 0.2)`
- accent 回落：`var(--color-accent, #58a6ff)`；正在看别的分支底色 `rgba(88,166,255,0.15)` / hover `rgba(88,166,255,0.25)`
- 拓扑图 8 色板（见 §5.4）
- 警示色回落 `var(--color-warning, #f59e0b)`（`GitWorktreeModal.tsx:797`）
- `var(--border-subtle, #3d3d3d)`（远程分支胶囊底，`GitHistoryContent.tsx:118`）

---

## 11. 坑与边界（总表，按严重度排序）

### ⚠️ 结构性（会导致返工）

1. **没有 pty-output 观察点**（§4.9）——Git 面板的自动刷新链路在 GPUI 侧**完全没有对应物**。
   `pane.rs:146-153` 的 reader 回调不外传字节。三条路见 §4.9，动手前先定方案。
2. **右抽屉现在是第三栏**（`main.rs:547-553`）——改悬浮层要动布局根节点、删掉 `columns_group` 第三个
   `resizable_panel`、改 `main.rs:554-580` 的 `on_resize` 回写逻辑（去掉 `sizes.get(2)` 那段），
   并自建拖拽手柄（`ResizableState` 拿不到了）。
3. **`AppStore::new_terminal` 不支持 cwd/title 覆盖**（`store.rs:372-397` 硬传 `None`）——
   仓库栏右键「在终端打开」与 worktree 行「开终端」两处都要。`spawn_pane` 本身已支持（`store.rs:800-812`），只差开口子。
4. **`AppStore::add_project` 不支持 parent 且不返回 id**（`store.rs:234-270`）——
   GitWorktreeModal 的「设为项目」要挂子项目。`docs/gpui-parity-audit.md:73` 已记为 store 缺失 action。
5. **阻塞调用必须丢 background executor**（§1.2）——`git_commit` **无超时**，主线程上跑一次 hook 慢的仓库就是永久卡死。

### ⚠️ 性能

6. **大 diff 全量渲染**（§6.6）——1MB 文本可出 ~20k 行，GPUI 全量建元素会卡。建议 `uniform_list`（行高恒定）。
7. **`discover_git_repos` 首次扫盘**——深度 5、跳过 7 类目录，但在大 monorepo 上仍是秒级。
   30s TTL 缓存在后端（`git.rs:294`），UI 侧**别再叠一层**；抽屉收着时**不要**触发。
8. **`get_changes_status` 每次全量**——`recurse_untracked_dirs(true)`，`node_modules` 未 gitignore 的仓库会很慢。
   500ms 去抖是原版的唯一保护。
9. **拓扑图逐行 SVG**——原版每行一个 `<svg>` 带自己的 `<defs>`。GPUI 侧建议整块一个 `PathBuilder` canvas，
   但**行高必须仍是 48px 固定**，否则连线跨行接不上。

### ⚠️ 正确性边界

10. **detached HEAD** —— `current_branch` 是 `"(1a2b3c4)"` 带括号（`git.rs:484-489`）。
    显示照旧，但**不能**把它当分支名传给 `get_git_log(branch=)`（会 `bail!("未找到分支:…")`）。
    分支徽章点击后 `viewBranch` 只从 `get_repo_branches` 的结果里取，所以天然安全——**别为了"方便"加一条从徽章文本取值的路径**。
11. **空仓库**（无 HEAD）—— `WT_NEW → Added`（不是 Untracked，`git.rs:110-114`），
    所以「未跟踪」分组会是空的、全落在「未暂存」。这是后端口径，UI 不必特判。
12. **二进制文件** —— `is_binary: true` 且 `hunks` 空。**必须先判 `isBinary` 再判 `tooLarge`**（原版顺序，`DiffModal.tsx:244-253`）。
13. **`tooLarge` vs `full_replace_diff`** —— 两条不同的降级路：
    >1MB → `too_large: true`（UI 显示"文件过大"）；
    ≤1MB 但 LCS 单元 >1000 万 → **仍返回 `too_large: false`**，只是 hunks 变成"整块删+整块加"。
    UI 对后者**无从区分**，会渲染一个巨大的全红全绿 diff。这是原版行为，照抄；若加提示属增强。
14. **重命名文件的 diff** —— `get_commit_file_diff` 必须传 `old_file_path`（`CommitDiffModal.tsx:57`），
    否则父树查不到旧内容 → 显示成"整文件新增"。
15. **非 git 项目** —— `discover_git_repos` 返回空 vec（不报错）。UI 侧：仓库栏整条不渲染，
    历史区显示 `gitHistoryContent.noRepos`，更改区因 `repoPath === ''` 而 `loadChanges` 直接 return（`GitChanges.tsx:115`）
    → 停在初始的"暂无变更"。**注意 GitChanges 的空 repoPath 分支既不 loading 也不报错**。
16. **SSH 远程项目** —— 整个 Git 面板换成一句占位（§3.2），**不要**试着跑本地 git。
17. **DiffModal 的 `staged` 依赖漏项**（§6.4）—— 原版 bug，GPUI 侧按三元组 key 自然修掉，建议顺修并注明。
18. **分页去重是必需的**（§5.2）—— 不去重会让拓扑图连线算错，且可能用同一游标死循环请求。
19. **迟到响应丢弃有两处**：`loadBranches` 按 `selectedRepoRef` 比对（`GitHistory.tsx:201`），
    `get_git_log` 按 `reqIdRef` 令牌（`GitHistoryContent.tsx:248`）。两套机制都要移植。
20. **worktree 删除的顺序**（§8.8）—— 先关终端、后删 git、成功了才删项目。顺序调换会在 Windows 上必失败。
21. **worktree prune 后必须 `filter_directories` 复核**（§8.9）—— `isValid=false` 但目录还在（元数据损坏）时项目要**保留**。
22. **`list_worktrees` 结果对主仓库与其内部 worktree 完全相同**（`GitWorktreeModal.tsx:145-146`）——
    不按 `isMain` 归并就会重复展示整组。
23. **`.git` 在 worktree 里是文件不是目录**（`git.rs:1048`）—— 任何"是不是仓库"的判断用 `.exists()`。
24. **`config.gitChangesViewMode` 是 String**（`mt-config/src/config.rs:135`）—— 手改坏值不能拖垮整份 config，
    解析失败回落 `"list"`（照 `locale` 字段的做法）。
25. **`sectionUi` 与 `collapsedDirs` 都是会话级、不落盘**（`GitHistory.tsx:13-15`、`GitChanges.tsx:106`）——
    别顺手写进 config。
26. **`historyRefreshKey` 会整体重建历史区**（`GitHistory.tsx:533`）—— 提交/pull/手动刷新后滚动位置与分页全丢。原版行为。
27. **两个 `GIT_REFRESH_PATTERNS` 副本**（`GitChanges.tsx:19` 与 `GitHistoryContent.tsx:188`）——
    GPUI 侧合成一份常量，但**两个面板各自去抖**（原版是两个独立的 500ms 定时器）。
28. **`git_stage_all` 会把未跟踪文件一起暂存**（`git.rs:1149` 的 `add_all("*")`）——
    所以「未跟踪」分组的 action 按钮虽写着"全部暂存"，实际是全仓库暂存（`GitChanges.tsx:397-400` 传的就是同一个 `handleStageAll`）。原版行为。
29. **同一文件可同时在 staged 与 unstaged 两组**（部分暂存）—— 行 key 必须带区名前缀
    （`` `${area}-${file.path}` ``，`GitChanges.tsx:238`），否则 GPUI 的 ElementId 会撞。
30. **`CommitFileInfo.status` 只有 4 种**（`git.rs:624-630`，其余全归 `"modified"`），
    但 `STATUS_LABELS` 仍留了 `?` 回落（`CommitDiffModal.tsx:99`）。

---

## 12. 实施建议

### 12.1 拆分顺序（每步可独立编译测试）

1. **右抽屉悬浮层化**（§2）——先把容器改对，Git 面板才有地方挂。
   验收：Sessions 抽屉浮在终端上、终端宽度不变、拖拽改宽落盘、segmented 切换动画正常。
2. **Palette 补 5 个 token**（§10.1）+ `overlay::kind` 补 4 个常量 + `USED_KEYS` 补 key（§9）。
3. **GitHistory 骨架 + 仓库栏**（§3）——含空态三分支、仓库/分支下拉、pull/push（丢后台）。
4. **GitChanges**（§4）——列表视图先行，树视图与右键菜单随后。
5. **DiffModal**（§6）——两个视图函数是 CommitDiffModal 的前置。
6. **GitHistoryContent + gitGraph**（§5）——拓扑图是本批最重的自绘件，建议先出无渐变版本。
7. **CommitDiffModal**（§7）。
8. **GitWorktreeModal**（§8）——最大件，依赖 store 的 `add_project(parent)` 扩展。
9. **ActivityBar Git 按钮**（§2.5）+ FileTree「查看变更」菜单项回填（`file_tree.rs:291`/`:777` 的注释一并删掉）。

### 12.2 单测建议（纯逻辑，不需要真仓库）

- `gitGraph::compute`：给定 parent_hashes 序列，断言 lane 分配、颜色继承、merge 判定、`width` 计算（**照抄 §5.4 的算法逐条对**）
- `SideBySideView` 的行配对算法（§6.6）：delete/add 不等长、纯 add、纯 context 三类输入
- `buildFileTree`（§4.4）：嵌套路径、同名目录与文件
- `sanitizeBranchForDir` / `normalizePath` / `joinPath` / `parentDir` / `baseName`（§8.2、§8.6）
- 分支交集算法（§8.5）：单组、多组、有占用、有远程
- `clampRatio` / 抽屉宽度 clamp
- `formatRelativeTime` 的 4 个档位边界（59/60/3599/3600/86399/86400/2591999/2592000）
- `GIT_REFRESH_PATTERNS` 命中/不命中样本
- `statusLabelFor` / `statusColor` 的 6 种 status × 3 个区

### 12.3 做完要更新的文档

- `docs/gpui-parity-audit.md:62`（#27 状态 → ✅ 并注提交号；**顺手把 BranchFamilyPanel 从该行划掉**，见 §0.2）
- `docs/gpui-parity-audit.md:70`（右抽屉悬浮层条目划掉）
- `docs/gpui-parity-audit.md:44`（#14 文件树「查看变更」已可用，从"剩"里去掉）
- `docs/gpui-parity-audit.md:69`（ActivityBar 的 Git 入口已补）
- `docs/gpui-migration-progress.md:16`（Wave 4+ 那行加本批的提交号）
- `docs/gpui-migration-progress.md` 技术债段：记 §4.9 选的方案、§10.1 diff 色的取舍、§6.6 的 uniform_list 改进

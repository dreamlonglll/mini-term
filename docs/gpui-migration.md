# mini-term GPUI 改造

把前端从 **Tauri v2 + WebView2 + xterm.js** 换成 **GPUI 原生渲染**，单进程、无 IPC。

分支 `gpui`，worktree `D:\Git\mini-term-gpui`，从 v0.13.1 (`e43ee4e`) 起步。

---

## 1. 依赖选型

| 依赖 | 版本 | 来源 | 许可 |
|---|---|---|---|
| `gpui` | 0.2.2 | crates.io（Zed 官方） | Apache-2.0 |
| `gpui-component` | 0.5.1 | crates.io（longbridge） | Apache-2.0 |
| `alacritty_terminal` | 0.26 | crates.io | Apache-2.0 |
| `portable-pty` | 0.8 | 沿用现状 | MIT |

全部走 crates.io，**不用 git 依赖**，版本可锁、构建可复现。

### 为什么不是 gpui-ce

调研起点是 [oxideterm](https://github.com/AnalyseDeCircuit/oxideterm)，它用的是 vendored 的
[gpui-ce](https://github.com/gpui-ce/gpui-ce)（社区 fork，把 Metal/D3D 后端换成 wgpu 单一代码路径）。
本项目**不跟这条路**，两条理由：

1. **gpui-component 依赖 Zed 官方 `gpui ^0.2.2`，不是 gpui-ce。** 两者混进同一棵依赖树
   会出现两个不同的 `gpui` crate，类型不互通。选 gpui-ce 就等于放弃全部现成组件，
   Resizable / Tree / TabBar / Modal / Table / 主题层全要自己写——oxideterm 正是这么做的，
   代价是一个 87 KB 的自研 `oxideterm-theme`。
2. **crates.io 上的 gpui-ce 停在 0.3.3 / 2025-12-27**，已停更 8 个月，且社区反馈它一度落后
   mainline 381 个提交。

gpui-ce 的唯一优势是 wgpu 单一后端；本项目以 Windows 为主要支持平台，Zed 官方的 Windows
后端正是 Zed 自身在用的路径，这个优势对我们不成立。

### oxideterm 的代码不可借用

该仓库 **GPL-3.0-only**。可以读它的架构与补丁说明（`crates/gpui-ce/gpui/OXIDETERM_PATCHES.md`
就是一份很好的踩坑清单），**不可复制任何实现**。

---

## 2. 工作区结构

根 `Cargo.toml` 的 `members = ["crates/*"]`，`exclude` 掉 `src-tauri` / `relay-server` /
`mobile` / `.tmp-tests`。**迁移期间新旧两套完全并存**：`npm run tauri dev` 与 CI 不受影响，
`cargo run -p mt-app` 跑新壳。

```
crates/
  mt-app        GPUI 应用壳（bin: mini-term）——窗口、三栏布局、Tab 与 SplitNode 树、全局状态
  mt-ui         GPUI 渲染层——TerminalElement、复用件、主题桥（无业务）
  mt-terminal   VT 状态机 + grid 模型（无 UI，不依赖 gpui）
  mt-pty        PTY spawn/read/write/resize/kill（无解析）
  mt-config     配置持久化 + 主题包
  mt-project    文件树 / 搜索 / Git / 外部编辑器 / WSL 枚举
  mt-ai         hook server / hook registry / 状态判定 / 会话记录
  mt-usage      用量统计（轮次解析 / SQLite ledger / 聚合 / 计价）
  mt-relay      移动端中转 + 对话镜像
```

数据流的核心分层：

```
mt-pty        字节进出子进程          （无解析）
mt-terminal   字节 → grid 状态        （无 UI）
mt-ui         grid 状态 → GPUI 元素   （无业务）
```

---

## 3. 迁移映射

### Rust 后端（21,363 行，大部分留用）

| 现有文件 | 行数 | 去向 |
|---|---:|---|
| `pty.rs` | 2297 | `mt-pty` + `mt-ai`（AI 命令识别与打断识别归 AI） |
| `conpty_bootstrap.rs` | 276 | `mt-pty`（原样搬，仍须早于任何 `openpty`） |
| `ai_sessions.rs` | 2123 | `mt-ai` |
| `hook_server.rs` | 1187 | `mt-ai` |
| `hook_registry.rs` | 1111 | `mt-ai` |
| `process_monitor.rs` | 571 | `mt-ai` |
| `config.rs` | 1298 | `mt-config` |
| `theme_packs.rs` | 429 | `mt-config` + 部分映射到 gpui-component 主题层 |
| `git.rs` | 1402 | `mt-project` |
| `fs.rs` | 868 | `mt-project` |
| `search.rs` | 461 | `mt-project` |
| `editor.rs` / `wsl_distros.rs` | 187 | `mt-project` |
| `usage_stats/*` + `aggregate/pricing/turns/ledger` | 3513 | `mt-usage` |
| `mobile_relay.rs` | 1233 | `mt-relay` |
| `mobile_mirror.rs` | 598 | `mt-relay` |
| `remote_ssh.rs` | 1281 | `mt-project`（**依赖 `mt-ssh`，见下方未决项**） |
| `clipboard.rs` | 286 | `mt-ui`（GPUI 自带剪贴板 API，多半是净删除） |
| `tray.rs` | 348 | `mt-app`（GPUI 的托盘支持需先验证） |
| `window_snap.rs` / `window_theme.rs` / `window_input_recovery.rs` | 597 | **多半整块删除**，见第 4 节 |
| `startup_trace.rs` | 45 | `mt-app` |
| `ssh*.rs`（registry / skill / mcp） | 1008 | `mt-project` 或独立 crate，迁移末期处理 |

### 前端（26,855 行 TS/TSX/CSS，全部重写）

| 现状 | GPUI 侧 |
|---|---|
| Allotment 三栏主布局 | `gpui_component::resizable` |
| 递归 SplitNode 树 | 同上嵌套；树结构本身是业务，留 `mt-app` |
| FileTree | `gpui_component::tree` |
| Tab 栏 | `gpui_component::tab` |
| 各类 Modal | `gpui_component::dialog` |
| Zustand store | GPUI `Entity` / `Global` |
| Tailwind v4 | GPUI 自带 Tailwind 风格 API（`.flex().gap_2().bg()`），迁移较顺 |
| 自研 zustand i18n | gpui-component 依赖 `rust-i18n`，字典从 `src/locales/*.ts` 转 |
| **xterm.js** | **无对应物，`mt-ui::TerminalElement` 自研** |

---

## 4. 净删除清单

这些代码在单进程 GPUI 架构下没有存在意义，迁移时**直接删掉，不要试图保留**：

| 删除项 | 原因 |
|---|---|
| PTY 的 16ms 批量缓冲 | 原为摊薄 `emit('pty-output')` 的 IPC 开销；现在没有 IPC |
| 有界 channel + 4MB/1MB 双水位背压 + `set_pty_flow_paused` + 30s 超时兜底 | 原为在 WebView 边界上人工造背压；现在解析速度即本进程速度，读慢了 ConPTY 自然阻塞刷屏进程 |
| `kill_all_ptys` 孤儿 PTY 回收 | 原为兜住 WebView2 renderer 被 OOM 杀掉后页面重载留下的孤儿 PTY；单进程不存在该场景 |
| `window_input_recovery.rs` | 原为修 WebView2 与外部工具争鼠标捕获 |
| WebView2 特有的字形测量兜底（v0.12.1 那套反射） | 换渲染器后该怪癖消失（但会换成 DirectWrite 的一套新怪癖） |

这是本次改造在后端侧最大的一笔净收益。

---

## 5. 已知坑位

从 oxideterm 的 `OXIDETERM_PATCHES.md` 反推——他们在 gpui 上打了 12 类补丁，其中这些**我们大概率也会撞上**：

| 坑 | 触发场景 | mini-term 是否涉及 |
|---|---|---|
| 重入 draw / element-arena 生命周期 | 剪贴板与拖放触发同步 window proc 重入 | ✅ 文件拖入终端 |
| DirectWrite 回调与 glyph 回读的指针安全 | Windows 文本渲染 | ✅ 全部文本 |
| Win32 线程池替换 WinRT 调度 | 后台任务与延迟执行 | ✅ 各类后台线程 |
| WGPU/设备丢失恢复（指数退避） | 虚拟机、远程桌面、驱动更新 | ⚠️ 视后端而定 |
| 透明 quad 的 GPU overdraw | 背景图 + 半透明叠层 | ✅ 背景图功能 |
| 嵌套滚动归属 | 内层滚动区不吞事件，父子同时滚 | ✅ 文件树嵌在可滚容器里 |

背景图另需自建缓存 + 内存预算（对应 oxideterm 的 `background_cache.rs` / `image_budget.rs`）。
GPUI 侧没有 CSS 的 `background-size` / `position` / `repeat` / `backdrop-filter`，
cover/contain 的 bounds 要自己算。

---

## 6. 验收项

这五条决定改造是否成立，越早验越好（原计划的 spike 内容，现在并入骨架阶段验收）：

1. **中英文混排逐列对齐** —— 复用 v0.12.1 那套「双终端对照页 + 截图逐列测量」诊断手法
2. **中文 IME 输入** —— 候选框定位、预编辑文本
3. **鼠标选择 + 剪贴板**
4. **文件从资源管理器拖入终端**
5. **背景图 + 半透明格子 + 运行时切主题 + `Blurred` 窗口**（Windows 上 Mica/Acrylic 成色未知）

任意两条卡死，就意味着要么自己维护 gpui fork（像 oxideterm 那样写自己的 PATCHES.md），
要么重新评估整条路线。

---

## 7. 未决项

- **`mt-ssh` / `mt-core` 的归属**：两者目前在 `src-tauri/` 下，还被 `mt-sidecars` 引用。
  等到迁移 `remote_ssh.rs` 时再一并挪进 `crates/`，同时改 `mt-sidecars` 的 path 依赖与
  `scripts/stage-sidecars.mjs`。**不要提前挪**——会打断现有 sidecar 构建管线。
- **Sixel / Kitty 图片协议**：`alacritty_terminal` 不支持。现有 xterm.js 侧若有依赖需单独评估。
- **托盘状态灯**：GPUI 的托盘支持程度未验证，必要时保留一个独立的平台层。
- **gpui-component 的 feature 裁剪**：默认 feature 会拉 23 个 tree-sitter 语言解析器，
  编译时间显著。骨架跑通后应关掉默认 feature 只留需要的。

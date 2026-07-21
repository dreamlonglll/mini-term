# 修复部分 Windows 机器 Codex CLI 无法滚动

## Goal

消除 mini-term 内运行 Codex CLI 时在部分 Windows 机器上无法形成 scrollback、因而无法滚动查看历史输出的问题，并恢复 Codex CLI 自己通过 ED3 清旧历史、重放最终 transcript 的自动折叠能力。Windows 兼容问题由固定版 ConPTY/OpenConsole 解决；折叠语义由 xterm 是否放行 ED3 决定。

## What I already know

* 问题只发生在部分 Windows 机器，同一程序在其他机器正常。
* CSS、xterm.js scrollback 配置、shell 差异和 `--no-alt-screen` 均已在用户的排查中排除。
* Codex CLI 会输出 ED2/ED3、DECSTBM、SU/RI 等 ANSI 控制序列；旧版系统 ConPTY/OpenConsole 可能无法稳定形成 scrollback。
* 用户给出的已验证方案是随应用侧载新版 ConPTY，并携带配套 `OpenConsole.exe`，在运行时优先加载，失败时回退系统默认实现。
* 本机 Codex CLI 0.144.6 的普通聊天 consolidation/reflow 会发送 `ED2 + ED3`，清除流式临时 scrollback 后重放 canonical transcript。
* mini-term 当前全局吞掉 ED3，导致旧临时行无法从 saved lines 删除；alternate-screen 47/1047/1049 拦截只影响 transcript/diff/审批等 overlay，不是普通聊天自动折叠的主因。

## Requirements

* 当前 x64 Windows 安装包必须携带同一官方 ConPTY 包中的 x64 `conpty.dll`，以及官方要求的 x64/ARM64 `OpenConsole.exe` host 目录。
* 创建本地 Windows PTY 时优先使用随包携带的便携 ConPTY/OpenConsole。
* 便携资源不存在、不完整或 DLL 预检失败时，不启用侧载目录，继续走 portable-pty 当前的系统 ConPTY 路径，不能阻止用户打开终端。
* 路径解析必须基于 Tauri 运行时资源目录，不能依赖当前工作目录。
* 开发环境、测试环境和已安装应用中的行为都应确定且可诊断。
* 保持现有 shell、cwd、环境变量、WSL 与 SSH 终端语义不变。
* 删除 mini-term 对 ED3（CSI 3J）的全局吞噬，让 xterm 默认 handler 执行标准 `Erase Saved Lines`。
* 保持 `scrollback: 100000`、focus 视口保护和 alternate-screen 47/1047/1049 拦截不变。
* ED3 恢复为全局标准语义：Codex、Claude、shell `clear` 或其他应用发送 ED3 时都可清除 saved lines，不以不可靠的 ANSI/进程启发式猜测发送者。

## Acceptance Criteria

* [ ] 自动化测试能在修复前对“便携 ConPTY 资源/选择策略缺失”给出失败，在修复后通过。
* [ ] 当前 Windows x64 发布目标能解析到 x64 `conpty.dll` 与官方要求的 x64/ARM64 `OpenConsole.exe` 资源树；未来目标映射不会静默选错架构。
* [ ] Tauri 构建配置会把资源放入最终应用包，且测试能防止漏打包或文件名/目录漂移。
* [ ] 本地 Windows PTY 优先尝试便携实现；资源缺失、不完整或 DLL 预检失败时回退系统实现。
* [ ] 回退不改变 PTY 创建 API，也不改变前端 xterm.js scrollback 配置。
* [ ] Rust 单测、前端类型检查/构建及适用的 Tauri 检查通过。
* [ ] 人工验证 Codex CLI 长输出可滚动、历史命令可回看，并覆盖至少一台原本会复现的机器。
* [ ] 自动化测试证明 ED3 不再被 mini-term 吞掉，ED0/1/2/3 均交给 xterm 默认实现。
* [ ] Codex 流式回答完成后旧临时内容消失，只保留重放后的最终 transcript；`/clear` 能真正清除旧历史。
* [ ] alternate-screen handler、10 万行容量和 focus 不回底行为没有随本次改动改变。

## Definition of Done

* 增加或更新单元/集成测试，覆盖资源定位、优先选择和回退策略。
* lint、typecheck、Rust tests/build checks 通过。
* 对分发物中的资源完整性进行验证。
* 将 Windows PTY 兼容性与回退约定记录到项目 spec。
* 同步 README 与前端 TUI scrollback spec，删除“拦截 ED3 保留历史”的过时公开承诺。

## Technical Approach

在 Rust/Tauri 启动边界集中实现“便携版优先、系统版兜底”的一次性策略。便携运行时作为 Tauri resources 随包分发，按官方进程/原生架构约定组织；启动时从应用资源目录解析并校验资源树，在第一次 `openpty()` 之前以绝对路径预载 DLL、校验兼容导出，并把模块引用保留到进程退出。Windows 后续按裸名加载时会先命中已加载模块，因此无需修改进程或子进程 PATH。资源校验或 DLL 预检失败时释放模块且不改变任何搜索环境，由 portable-pty 使用系统 ConPTY。由于 portable-pty 0.8.1 会进程级缓存首次选择，初始化必须发生在任何 PTY 创建之前，并通过一次性同步单元保证重复/并发调用只执行一次。

前端删除 `registerCsiHandler({ final: 'J' }, ...)` 的 ED3 覆盖，让 Codex 的 hard-reset/replay 进入 xterm 默认实现。1049 handler 保持原状，避免把“聊天折叠”和“全屏 overlay”两项产品语义绑在一次改动中。回归测试在 xterm 6.0.0 上写入 Codex 同形 `ED2 + ED3 + folded transcript`，断言旧 saved lines 被删除。

## Decision (ADR-lite)

**Context**：问题来自系统组件版本差异，修改 CSS、xterm.js、shell 参数或拦截 Codex 的 ED3/DECSTBM 序列都不能从根本上统一不同 Windows build 的行为。

**Decision**：随应用固定一套匹配架构的新版 ConPTY/OpenConsole 运行时，并在本地 Windows PTY 创建时优先使用；失败自动回退系统实现。

**Consequences**：安装包会增大，需要维护多架构 host 二进制与来源/版本；换来终端行为跨机器一致，且不侵入 Codex CLI 或 xterm.js。portable-pty 0.8.1 只在侧载 DLL 未被选中前支持系统回退；若 DLL 已加载而后续 `CreatePseudoConsole` 失败，现有 API 无法在同一进程重新选择系统函数表，此类失败继续按现有 PTY 创建错误返回，不伪装成已覆盖的回退场景。

**ED3 决策补充**：恢复 xterm 标准 ED3 语义，而不是尝试“仅 Codex 放行”。parser callback 没有发送者身份，现有 AI 状态也不区分 Codex/Claude；全局放行是可确定、可测试且与用户要求一致的最小实现。代价是所有应用都可永久清除 scrollback。

## Out of Scope

* 不修改 Codex CLI 或 `scrollback: 100000` 容量。
* 除移除 ED3 拦截外，不改写或延迟 ED2、DECSTBM、SU/RI 等控制序列。
* 不在运行时联网下载 ConPTY 资源。
* 不新增用户可见的 PTY 后端选择开关。
* 不把这次修复扩展为 WSL、SSH 远端 PTY 的后端改造。
* 不在本次新增 x86/ARM64 mini-term 发布目标；当前发布矩阵仍为 Windows x64。
* 不 fork/patch portable-pty 来实现“侧载 DLL 已选中后 CreatePseudoConsole 失败”的二次后端切换。
* 不在本次恢复 alternate-screen 47/1047/1049；Codex transcript/diff/审批 overlay 的原生行为另行评估。
* 不新增“仅 Codex”进程识别或设置 UI；若未来需要兼容“禁止应用清历史”，应以显式终端语义设置实现。

## Research References

* `research/pty-code-path.md` — 现有 PTY 创建链路与接入点。
* `research/portable-conpty.md` — portable-pty 侧载 API、资源约束与一手来源。
* `research/test-seam.md` — 自动化反馈环与打包验证方案。
* `research/codex-history-review.md` — 既有 Codex/xterm 滚动处理的 Git 历史复审与保留/调整建议。
* `research/codex-folding.md` — Codex 0.144.6 自动折叠的 ED3 证据、xterm 探针与作用域方案。

## Technical Notes

* 适用后端规范：`.trellis/spec/backend/index.md`、`.trellis/spec/backend/portable-pty-conpty-cwd-fallback.md`、`.trellis/spec/backend/pty-env-vars-injection.md`。
* 适用共享指南：`.trellis/spec/guides/cross-layer-thinking-guide.md`、`.trellis/spec/guides/code-reuse-thinking-guide.md`。
* 工作区已有未跟踪文件 `.trellis/workspace/dev/ssh-mcp-http-transport-analysis.md` 与本任务无关，必须保持不动。
* 原始滚动故障依赖特定 Windows build，当前开发机不保证可复现。自动化反馈环验证资源、架构、打包、启动选择与前置回退；受影响机器上的 Codex CLI 滚动行为仍是最终人工验收项。

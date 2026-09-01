# 01 — 前置重构：抽出「按启动器起会话」共享入口

**Parent:** issue #61（编排者 Orchestrator MVP）

**What to build:** 纯重构、零行为变化。"移动端发起的会话"在桌面侧的落地动作——校验启动器与项目 → 建 pane → 写入启动命令 → 回执——目前是移动端中转专用路径。把它抽成一个共享入口，供后续编排者的 start-session 复用（见 ADR 0003）。重构后移动端发起会话的全部既有行为保持一致：不抢焦点不切项目、一次性提示、启动失败 pane 保留不杀、SSH 远程项目与 WSL 根项目入口置灰（ADR 0002 边界一条不动）。

**Blocked by:** None — can start immediately.

**Status:** done（feff984，随合并 98749f9 入 main）

- [x] 移动端发起会话链路行为零变化，既有测试全绿
- [x] 共享入口不绑死中转体系的类型，编排控制面后续可直接调用（`AppStore::launch_ai_session` + `LaunchRequest`，`store/launch.rs`）
- [x] cargo test --workspace 全绿（合并后 28 目标复验）

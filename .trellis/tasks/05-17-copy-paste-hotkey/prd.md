# 终端复制粘贴快捷键：智能 Ctrl+C/V + 设置项

## Goal

GitHub issue #31：用户习惯 Ctrl+C/V 复制粘贴，不适应当前的 Ctrl+Shift+C/V。
采用「智能 Ctrl+C/V」方案 —— Ctrl+C 有选区时复制、无选区时透传 SIGINT；Ctrl+V 直接粘贴 ——
并在设置界面提供可控开关，让用户能开关该行为。

## What I already know

* 当前快捷键硬编码在 `src/utils/terminalCache.ts:248-261`：
  Ctrl+Shift+C → `copyTerminalSelection`，Ctrl+Shift+V → `pasteToTerminal`
* 复制/粘贴实现已完整且可复用：`copyTerminalSelection`（terminalCache.ts:385）、
  `pasteToTerminal`（:429，含长文本转存 + 图片转存逻辑）
* 配置系统：Rust `AppConfig`（config.rs，camelCase serde）+ 前端 `AppConfig`（types.ts），
  需前后端同步加字段；旧 config 兼容靠 `#[serde(default = ...)]`
* 设置界面 `SettingsModal.tsx`：5 页（terminal/system/ai-notification/shortcuts/about）
  - `TerminalSettings` 已有 toggle 开关样式可复用（如「长文本粘贴」）
  - `ShortcutsSettings` 的 `SHORTCUT_GROUPS` 硬编码了 Ctrl+Shift+C/V 说明
* 用户已决策：采用方案 1（智能 Ctrl+C/V），并要求完善设置界面

## Assumptions (temporary)

* 设置项以 toggle 形式加在终端设置页，风格与「长文本粘贴」开关一致
* 智能模式开启后保留 Ctrl+Shift+C/V（双轨并存，零破坏）
* 快捷键说明页需同步反映当前生效的快捷键

## Decision (ADR-lite)

**Context**：issue #31 要求可自定义复制粘贴快捷键；终端 Ctrl+C 默认是 SIGINT，不能简单改为复制。
**Decision**：智能 Ctrl+C/V（有选区复制、无选区透传 SIGINT），终端设置页 toggle 控制，
**默认关闭**；Ctrl+Shift+C/V 双轨保留。
**Consequences**：默认关闭 → issue 提出者需手动开启一次，但不改变任何现有用户的 Ctrl+C 行为，零回归。

## Requirements (evolving)

* Ctrl+C：终端有选区 → 复制并清除选区；无选区 → 透传给 PTY（SIGINT）
* Ctrl+V：直接走 `pasteToTerminal`（复用现有长文本/图片逻辑）
* 设置项持久化到 config.json，前后端 `AppConfig` 同步加字段
* 快捷键说明页同步反映当前生效的快捷键

## Acceptance Criteria (evolving)

* [ ] 开启智能模式后，选中文本按 Ctrl+C 复制到剪贴板
* [ ] 开启智能模式后，无选区按 Ctrl+C 能中断运行中的程序（SIGINT 透传）
* [ ] 开启智能模式后，Ctrl+V 粘贴（含长文本/图片转存）
* [ ] 设置项重启后保持
* [ ] 快捷键说明页文案与实际行为一致
* [ ] Rust cargo test 通过

## Definition of Done

* Rust cargo test 通过，前端 build/typecheck 通过
* 旧版本 config.json 能正常迁移（serde default）
* 行为变化在快捷键说明页有体现

## Out of Scope (explicit)

* 完整的快捷键自定义键位表（用户已否决方案 3）
* 复制/粘贴以外的其他快捷键改造

## Technical Notes

* `attachCustomKeyEventHandler` 返回 false 阻止 xterm 默认处理；返回 true 透传
* SIGINT 透传 = Ctrl+C 事件 return true，由 xterm 正常生成 \x03
* config.rs 加字段需配 `#[serde(default = ...)]` 保证旧 config 兼容
* 设置项 React 侧复用现有 `saveConfigPatch` 模式

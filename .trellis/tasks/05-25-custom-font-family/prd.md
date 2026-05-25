# 自定义字体（区分 UI 和终端）

## Goal

允许用户在「设置」里自定义两类字体：
1. **UI 字体** — 应用界面文本（含 `font-mono` 等宽位置如命令显示、kbd、版本号等）
2. **终端字体** — xterm.js 终端正文

两者当前均硬编码（终端在 `src/utils/terminalCache.ts:211`，UI 走 Tailwind / 系统默认）。本任务同时顺手解决 issue 32 的第二个反馈 —— WSL 启动后 p10k / starship prompt 里的 Nerd Font 图标变 tofu 方框（根因：默认 fallback 链无任何 NF 字体）。

## What I already know

- `AppConfig` 已有 `ui_font_size` / `terminal_font_size` 字段，新增 fontFamily 字段照葫芦画瓢
- `terminalCache.ts:211` 硬编码 `"'JetBrains Mono', 'Cascadia Code', Consolas, monospace"`，回退链无任何 NF
- UI 字号通过 `document.documentElement.style.fontSize = ${size}px` 实时应用（`SettingsModal.tsx:649`）
- 终端字号在 `TerminalInstance.tsx:168-172` 实时应用到已存在 term（`cached.term.options.fontSize = ...`）
- Rust `AppConfig` 用 `#[serde(default = "fn")]` 默认值处理旧 config 兼容
- `config.rs` 测试里有 JSON round-trip 和"old_config_without_layout"用例，加新字段需要 default 函数覆盖

## Assumptions (temporary)

- 至少两个字段：`uiFontFamily` + `terminalFontFamily`，由用户"区分 ui 和终端"得出
- UX 用单行文本输入框（CSS font-family 语法 `"X", "Y", monospace`），保持与现有字号 slider 同区域
- 改字体即时生效，不需要重启
- 空字符串回退到默认值

## Decision (ADR-lite)

**Context**: 需要确定字段粒度和默认值是否解 issue 32 第二问题。

**Decisions**:

1. **UI 字体粒度**：单字段 `uiFontFamily`，覆盖整个 UI 包括 `font-mono` 等宽位置。用户填 JetBrains Mono → 命令显示等位置照样等宽；用户填 sans-serif → 命令位置跟着变 sans-serif，可接受。
2. **默认终端 fallback 加 NF**：默认 `fontFamily` 改成 `"'JetBrainsMono Nerd Font', 'CaskaydiaCove Nerd Font', 'JetBrains Mono', 'Cascadia Code', Consolas, monospace"`。装了任一 NF 字体的用户自动得到 NF 图标，未装的用户回退到 JetBrains Mono，行为与现状一致。同时顺手解 issue 32 第二问题（p10k / starship NF 图标）。

**Consequences**:

- 字段更少（只 2 个 `uiFontFamily` / `terminalFontFamily`），配置简单。
- 用户若想让 UI 文本是 sans-serif、命令位置保持等宽，本方案无法满足 —— 留作未来需求。
- NF fallback 不会让未装 NF 的用户行为改变（因为字体回退），无破坏性。

## Requirements (locked)

- 设置 modal「外观」分区新增字体配置区（紧邻字号 slider）
- 两个字段独立可配：`uiFontFamily` / `terminalFontFamily`
- 改字体即时生效（无需重启）
- 空字符串回退到默认值
- 旧 config.json（无 fontFamily 字段）能正常加载
- 终端默认 fontFamily 包含 NF fallback

## Acceptance Criteria (evolving)

- [ ] 设置 modal 显示 UI / 终端 字体输入框（需 UI 验证）
- [ ] 输入 `Arial` 后 UI 字体立即变化（需 UI 验证）
- [ ] 输入 `"Cascadia Code"` 后所有终端字体立即变化（需 UI 验证）
- [ ] 清空输入框回退默认（需 UI 验证）
- [ ] 重启后配置持久化（需 UI 验证）
- [x] 旧 config.json 能正常加载（cargo test `font_family_absent_is_none` + `old_config_without_layout_deserializes` 覆盖）
- [x] Rust `cargo test` 全过（12/12）
- [x] `npm run build` TS 编译通过

## Definition of Done

- `cargo test` 全过（含 config 新增字段 round-trip）
- `npm run build` 前端构建通过
- 手动验证：即时生效 + 重启持久化 + 旧 config 兼容

## Out of Scope (explicit)

- 系统字体选择器（弹窗列出系统已装字体）— Tauri 拿系统字体列表需平台特定 API，留后续
- 字体粗细 / 字间距 / 行高 配置
- 不同 pane 用不同字体（全局统一）

## Technical Notes

涉及文件：

| 文件 | 改动 |
|---|---|
| `src-tauri/src/config.rs` | 新增 `ui_font_family` / `terminal_font_family` 字段 + 默认函数 + 更新 JSON 测试 |
| `src/types.ts` | `AppConfig` 同步新增字段（camelCase） |
| `src/components/SettingsModal.tsx` | 新增字体输入控件 + handler |
| `src/utils/terminalCache.ts` | `fontFamily` 从 config 读，默认值集中此处常量 |
| `src/components/TerminalInstance.tsx` | useEffect 实时同步 fontFamily 到 `cached.term.options` |
| `src/main.tsx` 或 `App.tsx` | 启动时应用 `uiFontFamily` 到 `document.documentElement.style.fontFamily` |

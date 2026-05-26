# 在 WSL 终端实现项目级环境变量注入

## Goal

让 mini-term 的 WSL 项目（cwd 为 `\\wsl$\<distro>\...` 或 `\\wsl.localhost\<distro>\...`）也能使用已有的项目级环境变量功能 —— 当前 ProjectConfig.envVars 在 WSL 分支被显式跳过（`pty.rs:676-687`），前端用黄色警告条让用户去 `~/.bashrc` 配置。本任务通过 WSLENV 机制 + cmd.env 配合，把宿主侧 envs 透传到 WSL 内 bash，让 WSL 项目和普通 cwd 项目在 envVars 体验上对齐。

## Research References

- [`research/wslenv-mechanism.md`](research/wslenv-mechanism.md) —— WSLENV 官方语义 + JetBrains/Windows Terminal/VS Code 等业界惯例 + 边界陷阱 + 三方案对比

## Requirements

### 后端 `src-tauri/src/pty.rs`

- 移除 WSL 分支「跳过 envs 注入」逻辑，改为：WSL 分支额外拼接 `WSLENV=K1/u:K2/u:...` 字符串注入到 wsl.exe 进程 env，同时按 v1 普通 cwd 路径相同的逻辑逐条 `cmd.env(k, v)`
- WSLENV flag 使用 `/u`（仅 Win→WSL 方向，不做路径翻译），与 JetBrains IDEA terminal 对齐
- 拼接 WSLENV value 时，**保留宿主已有 WSLENV**（先读 `std::env::var("WSLENV")`，存在则追加在尾部），避免覆盖用户在 PowerShell `$env:WSLENV` 或系统级设置
- MINITERM_* 前缀防御性过滤同样适用 WSL 分支（既不进 cmd.env 也不进 WSLENV 拼接）
- 用户 envs 里 `WSLENV` key **整条跳过**（防止覆盖 mini-term 拼出来的值）

### 前端 `src/components/ProjectEnvVarsModal.tsx`

- 把当前**黄色警告条**改成**绿色提示条**：「已通过 WSLENV 透传至 WSL bash（`/u` 单向，不做路径翻译）。值是路径时请填 Linux 风格如 `/home/u/...`；`.bashrc` 中 `export` 同名变量会覆盖此值」
- key 校验增加 `WSLENV`（大小写敏感）到 reject 名单（与 MINITERM_* 同级别保护）
- 错误文案：「`WSLENV` 由 mini-term 内部管理，不可用作变量名」

### 前端 `src/utils/projectEnv.ts`

- `getProjectEnvs` 无需改动（已过滤 enabled/空 key），后端拿到列表后自行处理 MINITERM_/WSLENV 过滤

### 现有 4 处 `create_pty` 调用

- 无需改动（TerminalArea ×2 + PaneGroup ×2 都已经传 `envs: getProjectEnvs(projectId)`）

### Spec 文档

- 更新 `.trellis/spec/backend/pty-env-vars-injection.md`：
  - WSL 章节从「v2 预留」升级为「已实现」
  - 补 WSLENV `/u` flag 默认、宿主 WSLENV 合并策略、`.bashrc` 覆盖语义说明
  - Wrong/Correct 矩阵补 WSL 分支条目
  - 更新前端 reject 名单：增加 `WSLENV`

### README

- 「项目管理」段补一句：「WSL 项目下环境变量通过 WSLENV 机制透传至 Linux bash（不做路径翻译；`.bashrc` 中 `export` 同名变量会覆盖）」

### 版本号

- package.json / Cargo.toml / tauri.conf.json / Cargo.lock 同步升级到下一个 patch（当前 0.4.14 → 0.4.15）

## Acceptance Criteria

### 核心功能（来自 PRD 初稿）

- [ ] AC-1: WSL 项目设置 `FOO=bar`（enabled），新开终端 `echo $FOO` 输出 `bar`
- [ ] AC-2: WSL 项目设置多个变量（含中文 value、特殊字符 value 如 `:` `=`），全部生效
- [ ] AC-3: WSL 项目设置 `enabled=false` 的变量，bash 内 `echo $KEY` 为空
- [ ] AC-4: WSL 项目（在尝试绕过前端校验场景下）设置 `MINITERM_FOO=x`，bash 内 `echo $MINITERM_FOO` 为空（后端二次防御）
- [ ] AC-5: 普通 cwd 项目行为不受影响（回归不破坏，注入顺序与 v1 一致）
- [ ] AC-6: WSL 检测口径与 `parse_wsl_unc` 完全对齐（4 种 UNC 形式 + host 大小写不敏感）

### Research 新增（来自 research/wslenv-mechanism.md §4.5）

- [ ] AC-NEW-1: 宿主 PowerShell 已 `$env:WSLENV="USER_FOO/u"` 时启动 WSL 终端，WSL 内 `echo $WSLENV` 看到 `K1/u:K2/u:USER_FOO/u`（mini-term 追加在前，宿主既有 WSLENV 保留在尾部，未被覆盖）
- [ ] AC-NEW-2: WSL `.bashrc` 末尾 `export FOO=from_bashrc`，项目设 `FOO=from_mini-term`，新开终端 `echo $FOO` 输出 `from_bashrc`（确认 `.bashrc` 覆盖语义，UI 文案已说明）
- [ ] AC-NEW-3: 项目设 `FOO=val:with:colons`（`/u` 默认 flag），WSL 内 `echo $FOO` 输出完整字符串 `val:with:colons`（确认 `:` 在 value 中不会被 WSLENV 解析切分）

### 前端校验

- [ ] AC-FE-1: 前端 modal 输入 `WSLENV` 作为 key 时显示错误「`WSLENV` 由 mini-term 内部管理，不可用作变量名」，保存按钮 disabled
- [ ] AC-FE-2: WSL 项目下，modal 顶部显示绿色提示条（非黄色警告），文案如上

## Definition of Done

- Rust 单测覆盖 WSLENV 字符串拼接逻辑（空列表、单个、多个、含 MINITERM_/WSLENV 过滤、宿主既有 WSLENV 合并）
- 手动 spot-check 清单：9 个 AC 场景在真机 WSL 验证（Ubuntu distro，PowerShell 启动 mini-term）
- spec 文档 `pty-env-vars-injection.md` 的 WSL 章节从 v2 预留升级为已实现，包含 wrong/correct 矩阵和 WSLENV 合并示例
- README 项目管理段补充 WSL 环境变量说明
- 版本号同步升级（package.json / Cargo.toml / tauri.conf.json / Cargo.lock 0.4.14 → 0.4.15）
- Lint / cargo check / typecheck 全部 green

## Decision (ADR-lite)

**Context**: WSL 项目下需要透传宿主侧自定义环境变量到 Linux bash，但 wsl.exe 进程 env 不会自动透传（v1 显式跳过，spec 预留 v2 方案 WSLENV）。Research 调研了 5 个业界类似工具实现。

**Decision**:
- **WSLENV flag** 选 `/u`（仅 Win→WSL 方向，不做路径翻译）—— 与 JetBrains IDEA terminal 对齐（最强业界背书）
- **路径转换** 不做（v2 范围外）—— mini-term 无法可靠区分用户填的"路径"是 Win 还是 Linux 风格
- **用户自定义 WSLENV** 加入 reject 名单（前端校验 + 后端跳过）—— 防止覆盖 mini-term 拼接的值
- **警告条** 改写为绿色提示条 —— 透传成功 + 边界说明（路径风格、`.bashrc` 覆盖）
- **宿主既有 WSLENV** 追加合并而非覆盖 —— 与 JetBrains/wslgit 标准做法对齐

**Consequences**:
- 优点：低用户教育成本（modal UI 几乎零改动），与业界最权威 IDE 对齐，路径不翻译让用户完全控制 value 形态
- 取舍：`.bashrc` 仍能覆盖透传变量（Linux shell 启动顺序决定，文案说明而非 bug）；高级用户想要 `/p`/`/l` 路径翻译需在 value 里自己写 `\$WSL_PATH` 等方式（v3 可作"高级"勾选扩展）
- 风险：宿主 WSLENV 合并时 mini-term 追加在前 `K1/u:K2/u:既有`，业界做法是放尾部，需确认顺序对优先级是否有实际影响（research 给出的 JetBrains 代码也是放尾部，跟进对齐）

## Out of Scope

- macOS/Linux 平台行为（本任务只影响 Windows + WSL 分支）
- WSL 内的 hook 协议透传（MINITERM_PTY_ID 等 → claude/codex hook）—— WSL VM 内 process_monitor 看不到子进程，需先做"WSL 内 AI 进程识别"才有意义
- Windows ↔ WSL 路径自动转换（`/p` `/l` flag）—— v3 可作"高级模式"勾选扩展
- 用户在 modal 选 flag 的高级 UI（每行 dropdown）—— 同上，v3 范围
- 自定义 WSL 内的登录 shell（仍用 distro 默认）
- WSL 数量上限的硬限制（30 条以上的软提示也不做）

## Technical Approach

### 后端 pty.rs 改动核心（伪代码，详见 research §4.4）

```rust
// 在 mini-term 内部 env 注入之后，用户 envs 注入之前
let user_envs: Vec<(String, String)> = envs.unwrap_or_default()
    .into_iter()
    .filter(|(k, _)| !k.starts_with("MINITERM_") && k != "WSLENV")
    .collect();

// WSL 分支额外拼 WSLENV value（/u flag），与宿主既有 WSLENV 合并（mini-term 在前）
if wsl_override.is_some() && !user_envs.is_empty() {
    let mut parts: Vec<String> = user_envs.iter()
        .map(|(k, _)| format!("{}/u", k))
        .collect();
    if let Ok(existing) = std::env::var("WSLENV") {
        if !existing.is_empty() {
            parts.push(existing);
        }
    }
    cmd.env("WSLENV", parts.join(":"));
}

// 实际 KV 注入（两个分支都走这步，这是与 v1 的关键差异）
for (k, v) in &user_envs {
    cmd.env(k, v);
}
```

### 前端校验改动核心

```typescript
// ProjectEnvVarsModal.tsx 的 validateKey 函数补一条
if (key === 'WSLENV') {
  return '`WSLENV` 由 mini-term 内部管理，不可用作变量名';
}
```

### 警告条改写

```tsx
// ProjectEnvVarsModal.tsx 第 200-204 行
{isWsl && (
  <div className="mx-5 mt-3 px-3 py-2 rounded bg-green-500/10 border border-green-500/30 text-sm text-green-200">
    ✓ WSL 项目环境变量通过 WSLENV 透传至 Linux bash（<code>/u</code> 单向，不做路径翻译）。
    值是路径时请填 Linux 风格如 <code>/home/u/...</code>；
    <code>~/.bashrc</code> 中 <code>export</code> 同名变量会覆盖此值。
  </div>
)}
```

## Implementation Plan (small PRs / single bundled PR)

按 [feedback_commit_workflow] 项目级习惯，倾向单 PR 收尾（避免拆分 churn）：

- 后端：pty.rs 重构 WSL 分支 envs 注入 + Rust 单测
- 前端：ProjectEnvVarsModal validateKey 加 WSLENV reject + 警告条改绿色提示
- spec：pty-env-vars-injection.md WSL 章节升级 + wrong/correct 矩阵
- docs：README 项目管理段补一句
- 版本号：0.4.14 → 0.4.15 同步升级
- 手动验证 9 条 AC 后提交

## Technical Notes

### 关键文件路径

- `src-tauri/src/pty.rs:599-687` —— WSL 分支与 envs 注入核心改动点
- `src-tauri/mt-core/src/wsl_path.rs` —— `parse_wsl_unc` 实现（无需改动，仅复用）
- `src/components/ProjectEnvVarsModal.tsx:76-204` —— 前端 WSL 检测 + 警告条 + key 校验改动点
- `src/utils/projectEnv.ts:7-15` —— `getProjectEnvs` helper（无需改动）
- `.trellis/spec/backend/pty-env-vars-injection.md` —— 现有合约文档，需更新 WSL 章节

### Research 关键发现

- WSLENV 起步 Windows 10 Build 17063 (1803, 2018-04)，Tauri v2 最低 1803 (build 17134)，**无需旧版兜底**
- WSL 1/2 共用同一份 init 代码（`microsoft/WSL/src/linux/init/util.cpp`），**行为一致**
- WSLENV value 仅含 name + flag，50 个变量 < 1KB，远低于 Win 32KB env block 上限，**无性能顾虑**
- Windows Terminal 的 `WSLENV.1` ~ `WSLENV.6` 六步注释（`ConptyConnection.cpp:55-148`）是最权威参考实现
- JetBrains `TerminalEnvironment.kt:69` 的 `envNamesToPass.map { "$it/u" }` 是 `/u` 默认策略的权威背书

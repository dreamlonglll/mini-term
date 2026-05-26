# 项目级环境变量功能

## Goal

为每个项目（ProjectConfig）支持自定义环境变量，启动该项目下的终端 PTY 时把这些变量注入到子进程（shell 及其后续子进程），方便不同项目隔离 PATH / API key / 构建标志等。

## Requirements

1. **数据模型**：`ProjectConfig` 新增 `envVars: Vec<ProjectEnvVar>` 字段，每条含 `key / value / enabled`（`enabled` 默认 true）。
2. **作用域**：仅项目级。系统环境变量由 portable-pty 默认继承父进程，项目级 `cmd.env(k, v)` 自然覆盖父进程同名变量。
3. **注入路径**：`create_pty` Tauri command 新增 `envs: Option<Vec<(String, String)>>` 参数；Rust 在已注入 `TERM/LANG/MINITERM_PTY_ID/MINITERM_HOOK_PORT` 之后追加用户变量。
4. **前端注入**：`TerminalArea.handleNewTab` / `handleSplitPane` 与 `PaneGroup.tsx`（含初次 hydration `useEffect` + `handleNewTab`）调用 `create_pty` 时，从当前项目 `envVars` 取出 `enabled === true` 的条目传入。
5. **UI 入口**：项目右键菜单新增"环境变量…"（仿 `SshAssocModal` 入口模式），打开独立 modal。
6. **校验规则**（严格 POSIX）：
   - key 必须匹配 `^[A-Za-z_][A-Za-z0-9_]*$`，非空。
   - key 不得以 `MINITERM_` 前缀开头（保护内部 hook 变量）。
   - 同项目内 key 不得重复。
   - value 不得含 NUL (`\0`) 或换行 (`\n` / `\r`)；空 value 允许。
7. **校验提示**：inline — 违规行的 key/value input 显示 `border-red-500`，下方一行小字描述错误（按优先级展示最严重一条：空 key > 含非法字符 > `MINITERM_` 前缀 > 重复 key > value 非法字符）；存在任一违规时**保存按钮 disabled**。
8. **modal 关闭行为**：✕ / Esc / "取消"按钮直接关闭丢弃修改；**点击遮罩不关闭**（防误触）。
9. **WSL 项目处理**：cwd 命中 WSL UNC 时，`create_pty` Rust 端在 `decide_wsl_override` 分支跳过 envs 注入（wsl.exe 进程 env 不透传给 Linux bash）；modal 顶部检测到 WSL 项目时显示黄色警告条："WSL 项目下环境变量暂不支持，请在 `~/.bashrc` 配置"。
10. **持久化**：`#[serde(default, skip_serializing_if = "Vec::is_empty")]`，旧 config.json 自动空 Vec，新空配置不写入文件。
11. **生效范围**：仅对**新建** PTY 生效，已有 PTY 不变；modal 底部明确文案提示。
12. **v1 不支持**：`${VAR}` 插值、`$(cmd)` 求值、`.env` 文件批量导入 / 粘贴、全局 envs、加密存储、UI masking、watch reload、行拖拽排序。

## Acceptance Criteria

- [ ] 项目 A 配 `FOO=bar`，新开终端 `echo $FOO` / `echo %FOO%` 输出 `bar`；项目 B 无 `FOO`，新开终端 `echo $FOO` 为空。
- [ ] 项目级 `PATH=C:\custom\bin;%PATH%` 能让 `where node` 找到自定义路径下的 node。
- [ ] 把某条 `enabled` 取消勾选 → 新终端不再带该变量，老终端不变。
- [ ] 保存 key `MINITERM_PTY_ID` 时该行 inline 红框 + 保存按钮 disabled。
- [ ] 输入两条同 key `FOO`，两行都红框、底部错误说明"key 重复"，保存按钮 disabled。
- [ ] key 含空格 / `=` / 中文 → 输入框红框 + 错误说明 + 保存 disabled。
- [ ] value 粘贴含换行的字符串 → 该行红框 + 保存 disabled。
- [ ] 旧 config.json（无 `envVars`）加载后 `envVars` 是空 Vec；保存空 Vec 时 JSON 中不出现该字段。
- [ ] Rust 单测：含 envVars 的 ProjectConfig serde round-trip；envVars 字段缺省时反序列化默认空。
- [ ] modal 中加 5 条变量、保存、关闭、重开 modal → 5 条原样回显，顺序不变。
- [ ] AI hook 子进程仍能拿到 `MINITERM_PTY_ID` / `MINITERM_HOOK_PORT`（用户未覆盖时）。
- [ ] WSL 项目（cwd 含 `\\wsl$\` 或 `\\wsl.localhost\`）打开 modal 时顶部显示黄色警告条；保存后新建终端中 `echo $FOO` 为空（envs 未透传给 Linux bash）。
- [ ] 点击 modal 遮罩区域不会关闭弹窗。
- [ ] 在 modal 编辑修改后按 Esc / ✕ / 取消 → 直接关闭，修改丢弃，不弹 confirm。

## Definition of Done

- Rust 单测覆盖 config serde + env 注入分支。
- 前端 lint / tsc 通过；构建无 warning。
- Windows 上手动验证以上 AC（至少 1/2/3/4）。
- 不破坏已有 PTY 行为（hook、WSL override、SSH 自动填充密码）。

## Out of Scope

- 全局 / 工作区级环境变量。
- 变量插值、命令求值。
- `.env` 文件导入或 watch reload。
- 敏感值加密或 UI masking（与现有 SSH password 明文存储一致）。
- 修改运行中 PTY 的环境变量（PTY 协议天然不支持）。
- 远端 SSH 会话注入环境变量（远端 shell 自有 env，不通过 PTY 启动注入）。

## Technical Approach

### 数据模型

```rust
// src-tauri/src/config.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEnvVar {
    pub key: String,
    pub value: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

pub struct ProjectConfig {
    // ... existing ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<ProjectEnvVar>,
}
```

> 为何用 `Vec` 而非 `HashMap`：保留用户编辑顺序、避免 JSON 序列化抖动、UI 中的"启用复选框"和"行级删除"都需要稳定索引。同 key 多条时取最后启用的一条（与 shell `export` 行为对齐）。

### 注入

```rust
#[tauri::command]
pub fn create_pty(
    // ...
    envs: Option<Vec<(String, String)>>,
) -> Result<u32, String> {
    // ... 已有 TERM/LANG/MINITERM_* 注入 ...
    if let Some(list) = envs {
        for (k, v) in list {
            cmd.env(k, v);
        }
    }
    // ... spawn ...
}
```

> 顺序很关键：用户变量在 MINITERM_* 之后注入，**理论上**用户能覆盖 MINITERM_*，但前端保存时已 reject `MINITERM_` 前缀的 key，等于双重保险。

### 前端

- 新增 `src/components/ProjectEnvVarsModal.tsx`，结构仿 `SshAssocModal`：
  - 上方说明文字："修改后仅新建终端生效；已有终端不受影响。"
  - 表格行：`[enabled checkbox] [key input] [value input] [删除按钮]`
  - 底部："+ 新增一行"、"取消"、"保存"。
- `ProjectList.tsx` 右键菜单加 "环境变量…" 项，紧挨 "关联 SSH…"。
- `TerminalArea.tsx` 与 `PaneGroup.tsx` 在调用 `create_pty` 处：
  ```ts
  const envs = (project.envVars ?? [])
    .filter((e) => e.enabled && e.key.trim())
    .map((e) => [e.key, e.value] as [string, string]);
  invoke('create_pty', { shell, args, cwd, envs });
  ```

### 兼容

- 旧 `config.json` 无 `envVars` → `serde(default)` 给空 Vec。
- 空 Vec 时 `skip_serializing_if` 保持 JSON 干净（不出现该字段）。
- 新增 2 条单测：含 envVars round-trip、缺省字段反序列化。

## Decision (ADR-lite)

**Context**：mini-term 需要让不同项目在终端内拥有独立的环境（PATH / API key / 构建标志），避免污染全局或互相干扰。

**Decision**：
- 仅做项目级，不引入全局 envs（系统级已通过父进程继承覆盖）。
- 数据用 `Vec<{key, value, enabled}>` 保留顺序。
- UI 仿 `SshAssocModal` 独立 modal，右键菜单入口。
- v1 纯字符串值，不做 `${VAR}` 插值。
- 保存时拒绝 `MINITERM_` 前缀 key 防止覆盖内部变量。

**Consequences**：
- (+) 实现最小化，迭代风险低，与现有 modal 模式一致。
- (+) portable-pty 默认继承机制让"普通环境变量自动可用"成为零成本。
- (-) 未来若要"全局 envs"或"工作区级"，需要在 config / 注入路径 / UI 三处加层；但因 `Vec<(k,v)>` 注入接口是平铺的，前端做 merge 即可，后端不动。
- (-) 不支持插值意味着不能 `PATH=${HOME}/bin:${PATH}` —— 用户必须用 shell 自身的 expansion（`$PATH` / `%PATH%`），跨平台心智略增；可在 v2 引入。

## Technical Notes

- `portable-pty::CommandBuilder::env(k, v)` 文档：默认继承父进程 env，逐条 `env()` 调用只追加/覆盖单条。
- 现有 PTY 内部变量（必须保护）：`MINITERM_PTY_ID`、`MINITERM_HOOK_PORT`、以及标准的 `TERM`、`COLORTERM`、`LANG`、`LC_CTYPE`、`LESSCHARSET`（用户若覆盖 `TERM` 可能导致 xterm 渲染错乱，但这是用户自找，仅 `MINITERM_*` 是 mini-term 内部约定，强制保护）。
- `SshAssocModal` 代码位于 `src/components/SshAssocModal.tsx`，可作为 UI 模板参考。
- `create_pty` 现有调用点：`TerminalArea.tsx:58`、`TerminalArea.tsx:104`、`PaneGroup.tsx:58`、`PaneGroup.tsx:97`（4 处都要改）。
- 前端 store 已有 `useAppStore` 暴露 `config`，可直接 `config.projects.find(p => p.id === projectId)?.envVars`。
- 注入参数命名：Tauri command 参数前端传 `envs`（camel），后端 `envs: Option<Vec<(String,String)>>`（snake 与 camel 在单词上一致，无需 rename）。

## Implementation Plan (small PRs / commits)

1. **后端 schema + 注入**：`config.rs` 加 `ProjectEnvVar` + `ProjectConfig.env_vars`，2 条 serde 单测；`pty.rs` 的 `create_pty` 加 `envs` 参数并注入。
2. **前端类型 + modal**：`types.ts` 加类型；新建 `ProjectEnvVarsModal.tsx`（仿 SshAssocModal）；`ProjectList.tsx` 右键菜单新增入口。
3. **前端注入 + store 操作**：`TerminalArea.tsx` / `PaneGroup.tsx` 4 处调用注入 envs；store 加 `updateProjectEnvVars` action。
4. **校验 + UX 打磨**：保存校验（key 合法、非 MINITERM_ 前缀、非空）、错误提示、modal 底部生效范围说明。

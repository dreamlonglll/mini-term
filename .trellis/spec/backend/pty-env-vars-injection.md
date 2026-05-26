# PTY 子进程环境变量注入契约

> 给 `create_pty` 注入项目级用户环境变量的完整契约。前端校验 + Rust 端注入 + WSL 分支跳过 + `MINITERM_*` 双重保护。后续扩展（全局 env / WSLENV 翻译 / `.env` 导入）必须沿用此契约。

---

## 1. Scope / Trigger

- **新 command 参数**：`create_pty` Tauri command 新增 `envs: Option<Vec<(String, String)>>`。
- **跨层契约变更**：前端 `ProjectConfig.envVars` → `getProjectEnvs(projectId)` → Tauri payload `envs` → Rust `CommandBuilder::env`。
- **内部保留变量保护**：`MINITERM_PTY_ID` / `MINITERM_HOOK_PORT` 是 hook 子进程协议的一部分，前后端各一道防护。
- **WSL 分支条件性跳过**：cwd 命中 WSL UNC 时,wsl.exe 进程 env **不会**自动透传给 Linux bash,注入了反而误导用户。

凡是要"对 PTY 子进程额外注入变量"（未来的全局 env、`.env` 文件导入、远端 SSH session env 等），都必须复用本契约的注入路径与保护规则。

---

## 2. Signatures

### Rust 端

```rust
// src-tauri/src/pty.rs
#[tauri::command]
pub fn create_pty(
    app: AppHandle,
    state: tauri::State<'_, PtyManager>,
    hook_state: tauri::State<'_, crate::hook_server::HookState>,
    shell: String,
    args: Vec<String>,
    cwd: String,
    envs: Option<Vec<(String, String)>>,  // ← 新增
) -> Result<u32, String>;
```

### 前端调用

```ts
// src/utils/projectEnv.ts
export function getProjectEnvs(projectId: string): Array<[string, string]>;

// 4 处调用点：src/components/{TerminalArea,PaneGroup}.tsx
invoke<number>('create_pty', {
  shell, args, cwd,
  envs: getProjectEnvs(projectId),  // ← 二维数组 [[k,v],...]
});
```

### 持久化

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
    // ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<ProjectEnvVar>,
}
```

---

## 3. Contracts

### Request 字段（前端 → Rust）

| 字段 | 类型 | 约束 |
|---|---|---|
| `envs` | `Option<Vec<(String, String)>>` | `None` / 空 Vec / 列表均合法;同一个 list 中允许重复 key（**后入覆盖前入**，与 shell `export` 语义对齐） |

序列化形态：JS 端 `[["FOO","bar"],["PATH","..."]]` ↔ Rust `Vec<(String, String)>`（Tauri 默认 serde 处理 tuple 为 JSON 数组）。

### 持久化字段（`config.json`）

```jsonc
{
  "projects": [{
    "id": "...",
    "envVars": [                   // 空 Vec 时不出现该字段
      {"key": "FOO", "value": "bar", "enabled": true},
      {"key": "DEBUG", "value": "1", "enabled": false}
    ]
  }]
}
```

- `envVars` 整体可缺省（`#[serde(default)]` → 空 Vec）。
- 空 Vec **不写盘**（`skip_serializing_if = "Vec::is_empty"`），保持配置文件干净。
- `enabled` 缺省 → `true`（`default = "default_true"`），允许老 JSON 漏写该字段。

### 注入顺序契约（Rust 端，**不可调换**）

```
1. cmd.env("TERM", "xterm-256color")
2. cmd.env("COLORTERM", "truecolor")
3. cmd.env("LANG", "C.UTF-8")          ┐
4. cmd.env("LC_CTYPE", "C.UTF-8")      │ 标准终端 / locale,用户可覆盖
5. cmd.env("LESSCHARSET", "utf-8")     ┘
6. cmd.env("MINITERM_PTY_ID", ...)     ┐ 内部 hook 协议,用户不可覆盖
7. cmd.env("MINITERM_HOOK_PORT", ...)  ┘
8. 用户 envs（按列表顺序，逐条 cmd.env(k, v)）
   └ 跳过 `MINITERM_` 前缀的 key（Rust 端防御性过滤）
   └ WSL override 命中时整段跳过（wsl.exe env 不透传给 Linux）
```

**关键点**：用户 envs 在第 8 步注入,**可以**覆盖 TERM / LANG（这是 feature，用户合理覆盖场景），**不能**覆盖 `MINITERM_*`（步骤 8 的过滤 + 前端校验双重保护）。

### Environment keys（受保护，约定不允许用户覆盖）

| Key | 来源 | 保护手段 |
|---|---|---|
| `MINITERM_PTY_ID` | `pty.rs` 第 6 步 | 前端 modal reject + Rust `starts_with("MINITERM_") → continue` |
| `MINITERM_HOOK_PORT` | `pty.rs` 第 7 步 | 同上 |
| `MINITERM_*` 任意前缀 | 预留 | 同上（防御未来新增） |

---

## 4. Validation & Error Matrix

### 前端校验（modal 保存前）

| 条件 | 错误码 | UI 表现 |
|---|---|---|
| key 为空（trim 后） | `empty-key` | 行红框 + "key 不能为空" |
| key 以 `MINITERM_` 开头 | `protected-prefix` | 行红框 + "MINITERM_ 前缀为内部保留" |
| key 不匹配 `^[A-Za-z_][A-Za-z0-9_]*$` | `invalid-key` | 行红框 + "key 只能含 a-z A-Z 0-9 _,首字符不能是数字" |
| 同项目内 key 重复 | `duplicate-key` | 涉及的所有行都红框 + "key 与其他行重复" |
| value 含 `\n` / `\r` / `\0` | `invalid-value` | 行红框 + "value 不能含换行或 NUL 字符" |

**优先级**：空 key > 受保护前缀 > 非法字符 > 重复 > value 非法（同一行只显示最严重那条）。

**任一违规** → 保存按钮 `disabled`。

### Rust 端二次防护

不做完整校验（信任前端），但对 `MINITERM_*` 前缀做硬过滤：

```rust
if wsl_override.is_none() {
    if let Some(list) = envs {
        for (k, v) in list {
            if k.starts_with("MINITERM_") {
                continue;  // 即便用户手改 config.json 绕过 UI,也守住 hook 协议
            }
            cmd.env(k, v);
        }
    }
}
```

为何不在 Rust 做完整 POSIX 校验：portable-pty 自身对非法 env 会报错或忽略，重复一份校验反而散乱；唯一**必须**守住的是 `MINITERM_*`，因为破坏 hook 关联是静默失败（用户不知道为何 AI 状态不工作）。

### 前后端 WSL 检测口径

前端 `isWslPath` 必须与 Rust `mt_core::parse_wsl_unc` **完全等价**（包括 verbatim 形式与大小写不敏感 host）：

```ts
// src/components/ProjectEnvVarsModal.tsx
function isWslPath(path: string): boolean {
  const afterPrefix = path.startsWith('\\\\?\\UNC\\')
    ? path.slice('\\\\?\\UNC\\'.length)
    : path.startsWith('\\\\')
      ? path.slice(2)
      : null;
  if (afterPrefix === null) return false;
  const sep = afterPrefix.indexOf('\\');
  if (sep <= 0) return false;
  const host = afterPrefix.slice(0, sep).toLowerCase();
  return host === 'wsl$' || host === 'wsl.localhost';
}
```

口径不一致的后果：Rust 跳过注入但 UI 不警告 → 用户保存后困惑为何 `echo $FOO` 没值。

---

## 5. Good / Base / Bad Cases

### Good（合法 + 成功注入）

- 项目 A 配 `FOO=bar`（enabled=true）→ 新建终端 `echo $FOO` / `echo %FOO%` 输出 `bar`
- 项目 A 配 `PATH=C:\custom\bin;%PATH%` → 新建 cmd 终端 `where node` 找到 `C:\custom\bin\node.exe`
- 项目 A 配 `LANG=en_US.UTF-8`（覆盖默认 `C.UTF-8`）→ 新建终端 git CLI 输出英文
- 项目 A 配 5 条变量，cancel 4 条 enabled，保存 → 新终端只带剩下的 1 条

### Base（合法但行为有边界）

- 同一 envs list 含 `FOO=a` 与 `FOO=b` 两条 → portable-pty `CommandBuilder` 后者覆盖前者，子进程看到 `FOO=b`（前端 modal 会先 reject 重复，仅当通过编辑 config.json 绕过才会出现）
- 项目 A 路径是 `\\wsl$\Ubuntu\home\u\proj` → 即便配了 `FOO=bar`，新建 WSL 终端 `echo $FOO` 为空（Rust 跳过注入；modal 顶部黄色警告条提示）
- 项目 A 配 `enabled=false` 的 `API_KEY=sk-xxx` → 不注入但 value 保留在 config.json，下次打开 modal 还能看到（取消勾选保留 value 即此用途）
- 旧 `config.json` 无 `envVars` 字段 → `serde(default)` 给空 Vec，加载后行为等价"该项目无 env 配置"

### Bad（被前端 / Rust 拦截）

- 前端输入 `MINITERM_PTY_ID=999` → modal 行红框 + 保存 disabled（前端 reject）；即便用户手改 config.json 注入，Rust 端 `starts_with("MINITERM_")` 也会 continue 跳过
- 前端输入 `1FOO=bar`（数字开头） → modal 行红框 + 保存 disabled
- 前端输入 `FOO BAR=baz`（key 含空格） → modal 行红框 + 保存 disabled
- value 粘贴含 `\n` 的多行文本 → modal 行红框 + 保存 disabled（避免 unix env 截断 / 跨平台不一致行为）
- 配 `MINITERM_HOOK_PORT=9999` 想故意破坏 hook → 前端 reject + Rust 跳过 = 静默无效化（hook 仍正常工作）

---

## 6. Tests Required

### 后端单测（`src-tauri/src/config.rs`）

`#[test] fn env_vars_round_trip` —— assertion 点：
- 反序列化 3 条 envVars（含缺省 enabled 字段，验证默认 true）
- 序列化后再反序列化保持完整、顺序、value 不变

`#[test] fn env_vars_absent_is_empty_and_not_serialized` —— assertion 点：
- 旧 JSON 无 `envVars` → `projects[0].env_vars.is_empty()`
- 空 Vec 序列化结果 `!contains("envVars")`（验证 `skip_serializing_if`）

### 应补但**未做**（v1 范围外）

- `create_pty` 注入路径单测：完整 spawn 需要真实 PTY，pty.rs 现有测试都是 `PtyManager` 输入处理的纯逻辑测试，env 注入需 e2e（spawn `printenv` / `set` 解析输出）。**手动 spot-check 替代**。
- `MINITERM_*` 过滤的 Rust 单测：同上，需 spawn 真实进程才能验证 child env，暂无。

### 前端

- 项目无前端 jest setup，靠 `npx tsc --noEmit` + `npm run build` 兜底类型与编译错误。

### 手动 spot-check 清单

1. 配 `FOO=bar` → `echo $FOO` 看 `bar`
2. 不勾 enabled → 新终端不带该变量
3. 输入 `MINITERM_X=1` → 行红框 + 保存 disabled
4. 输入两条同 key → 两行红框
5. WSL 项目 `\\wsl$\Ubuntu\...` 打开 modal 见黄色警告条；保存后新建终端 `echo $FOO` 为空
6. Esc / ✕ 关 modal 丢弃；点遮罩无反应

---

## 7. Wrong vs Correct

### Wrong：仅靠前端校验保护 `MINITERM_*`

```rust
// pty.rs（错误版本：信任前端，无 Rust 端过滤）
if wsl_override.is_none() {
    if let Some(list) = envs {
        for (k, v) in list {
            cmd.env(k, v);  // ❌ 用户手改 config.json 即可覆盖 MINITERM_PTY_ID
        }
    }
}
```

后果：用户编辑 `config.json` 塞 `MINITERM_PTY_ID=999`，hook 子进程收到错的 pty_id，AI 状态关联到错误终端 / 完全丢失关联，且**用户无任何提示**（hook 是静默失败的）。

### Correct：Rust 端兜底保护（双重防护）

```rust
if wsl_override.is_none() {
    if let Some(list) = envs {
        for (k, v) in list {
            if k.starts_with("MINITERM_") {
                continue;  // ✅ 守住 hook 协议,即便前端被绕过
            }
            cmd.env(k, v);
        }
    }
}
```

### Wrong：WSL 分支注入 envs 期望 Linux bash 拿到

```rust
// 错误想法：reach Linux 内的 bash
let mut cmd = CommandBuilder::new("wsl.exe");
cmd.args(["-d", "Ubuntu", "--cd", "/home/u/proj"]);
cmd.env("FOO", "bar");  // ❌ 这是 wsl.exe 进程的 env,不会透传给 Linux bash
spawn(cmd);
```

WSL 的环境变量透传机制是 `WSLENV`（声明白名单 + 类型标志），不是简单的进程 env 继承。直接 `cmd.env` 注入到 wsl.exe，Linux 端 bash `echo $FOO` 仍为空。

### Correct：WSL 分支跳过注入并在 UI 警告

```rust
// pty.rs
if wsl_override.is_none() {  // ✅ 仅普通 cwd 才注入用户 envs
    if let Some(list) = envs {
        for (k, v) in list {
            if k.starts_with("MINITERM_") { continue; }
            cmd.env(k, v);
        }
    }
}
// WSL 分支：用户 envs 完全忽略,前端 modal 顶部黄色警告条
```

未来若做 WSLENV 翻译（v2），改这里：把 user envs 转成 `WSLENV=FOO/u:BAR/u` 形式 + 同时 `cmd.env("FOO", "bar")`。

### Wrong：前端 `isWslPath` 漏 verbatim 前缀

```ts
function isWslPath(path: string): boolean {
  // ❌ 只查普通 \\wsl$\,漏 \\?\UNC\wsl$\
  return path.startsWith('\\\\wsl$\\') || path.startsWith('\\\\wsl.localhost\\');
}
```

后果：Rust `parse_wsl_unc` 识别 `\\?\UNC\wsl$\...` 跳过 envs 注入，但前端 modal 不显示警告条 → 用户保存后困惑。

### Correct：与 Rust `parse_wsl_unc` 口径完全对齐

```ts
function isWslPath(path: string): boolean {
  const afterPrefix = path.startsWith('\\\\?\\UNC\\')
    ? path.slice('\\\\?\\UNC\\'.length)
    : path.startsWith('\\\\')
      ? path.slice(2)
      : null;
  if (afterPrefix === null) return false;
  const sep = afterPrefix.indexOf('\\');
  if (sep <= 0) return false;
  const host = afterPrefix.slice(0, sep).toLowerCase();  // ✅ host 大小写不敏感
  return host === 'wsl$' || host === 'wsl.localhost';
}
```

---

## 真实出处

`05-26-project-env-vars` 任务落地。trellis-check 阶段发现两个阻塞问题（前端 isWslPath 漏 verbatim、Rust 缺 `MINITERM_*` 二次保护）已修复。完整 PRD 与 grill 决策见 `.trellis/tasks/05-26-project-env-vars/prd.md`。

相关 spec：
- `wsl-exe-cd-path-semantics.md` — wsl.exe `--cd` 路径语义、distro 识别规则
- `windows-unc-verbatim-prefix-strip.md` — verbatim 前缀剥离规则
- `agent-config-injection.md` — Claude / Codex hook 注入约定（`MINITERM_PTY_ID` 的消费方）

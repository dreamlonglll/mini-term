# Research: WSLENV 机制与业界惯例

- **Query**: WSLENV 官方语义 + 业界类似工具实践 + 边界 + mini-term 技术选型
- **Scope**: external (Microsoft 官方 + 主流 IDE/Terminal 源码)
- **Date**: 2026-05-26

---

## 1. WSLENV 官方语义

### 1.1 起源与版本可用性

- WSLENV 在 **Windows 10 Build 17063 (1803, 2018-04)** 开始支持,该 build 之前 WSL 只能从宿主继承 `PATH`,没有任何途径让宿主向 WSL 注入自定义 env(devblog 原文 "Prior to 17063, only Windows environment variable that WSL could access was PATH")。
- mini-term 依托 Tauri v2,Tauri v2 最低支持 **Windows 10 1803 (build 17134)**(高于 17063),因此 **WSLENV 必然可用,不需要老版本兜底**。
- `wsl.exe` **没有** `--env` / `--env-file` 等专门传环境变量的 CLI 参数(2026 年最新文档已确认,参考 `wsl --help` 列表)。WSLENV 是宿主向 distro 传变量的**唯一官方机制**。

### 1.2 语法

```
WSLENV=NAME1[/flags]:NAME2[/flags]:NAME3...
```

- 冒号 `:` 分隔多个变量名(**与 Linux PATH 同分隔符**,Win32 端也写 `:`,不是 `;`)。
- 每项格式:`变量名` 或 `变量名/flags`。
- **WSLENV 大小写敏感**(官方文档明确 "WSLENV is case sensitive"),变量名匹配也大小写敏感。
- 变量本身的 value 不在 WSLENV 里,WSLENV 只列**名字 + 翻译标志**;宿主进程的 env block 里同时要有对应变量的实际 value。

### 1.3 4 个 Flag 精确语义

源代码出处:`microsoft/WSL` 仓库 `src/linux/init/util.cpp::UtilParseWslEnv`(WSL 启动 distro 时由 init 进程解析,见研究中提取的 2110-2265 行)。逻辑:

```cpp
Reverse = (NtEnvironment != nullptr);  // 在 WSL 内由 Linux→Win 调用时为 true
case 'p': // path,Win<->WSL 路径翻译
case 'l': // path list,; ↔ : 同时翻译每段路径
case 'u': // 仅当 Reverse==false (Win→WSL) 时保留;否则 SkipTranslation
case 'w': // 仅当 Reverse==true (WSL→Win) 时保留;否则 SkipTranslation
default:  // 未知 flag → SkipTranslation = true (整个变量被丢弃)
```

| Flag | 含义 | Win→WSL 方向 | WSL→Win 方向 | 路径翻译 |
|---|---|---|---|---|
| 无 flag | 双向透传,字符串原样 | 透传 | 透传 | 不翻译 |
| `/p` | 单路径翻译 | `C:\foo` → `/mnt/c/foo` | `/home/u/x` → `\\wsl$\<distro>\home\u\x` | 翻译 |
| `/l` | 路径列表翻译 | `C:\a;D:\b` → `/mnt/c/a:/mnt/d/b` | `/a:/b` → `\\wsl$\...\a;\\wsl$\...\b` | 翻译且改分隔符 |
| `/u` | 仅 Win→WSL 方向有效 | 透传 | **丢弃** | 不翻译 |
| `/w` | 仅 WSL→Win 方向有效 | **丢弃** | 透传 | 不翻译 |

### 1.4 Flag 组合规则(关键)

- **`/p` 和 `/l` 互斥**:源码 `if (Mode != 0 && Mode != 'p' && Mode != 'l') SkipTranslation = true`。如果同时写 `/pl` 或 `/lp`,后一个覆盖前一个,但若与已选 mode 冲突会被丢弃。
- **方向 flag 可叠加路径 flag**:`/up`(仅 Win→WSL,翻译单路径)、`/wp`(仅 WSL→Win,翻译单路径)、`/ul`、`/wl` 均合法,**官方 devblog 示例就用了 `WSLENV=FORWSL/up`**。
- **未知 flag 整条丢弃**(实现是 `default: SkipTranslation = true`)。例如 `FOO/x` 会被静默扔掉,**不会**报错。

### 1.5 wsl.exe 透传机制

从 `microsoft/terminal` 的 ConPTY 实现(`src/cascadia/TerminalConnection/ConptyConnection.cpp:55-148`)可以看到**业界标准做法**:

```cpp
// 1. 在宿主进程 env block 中放入实际 KV
environment.set_user_environment_var(key, value);
// 2. 同时把 key 追加进 WSLENV(无 flag = 双向透传)
additionalWslEnv.append(key);
additionalWslEnv.push_back(L':');
// 3. CreateProcess 传 lpEnvironment
```

**结论**:
- **必须同时设置 cmd.env 和 WSLENV**:cmd.env 提供 value,WSLENV 声明哪些 name 要透传。
- 只设置 cmd.env 不在 WSLENV 列名 → **Linux 端 bash 看不到该变量**(已被 mini-term 当前 v1 实测验证)。
- 只设置 WSLENV 列名但宿主 env 里没有对应 KV → `UtilGetEnv` 返回空,WSL 端拿到空字符串。
- **WSLENV 自身不需要在白名单里**,WSL init 主动读取宿主进程的 `WSLENV` env var(源码 `UtilGetEnv(WSLENV_ENV, NtEnvironment)`),无需声明自传。

### 1.6 优先级 / 覆盖规则

来自 devblog FAQ 原文:

| 场景 | 行为 |
|---|---|
| Win 设了 WSLENV 透传 `FOO=A`,WSL 的 `.bashrc` 也 `export FOO=B` | **bash 启动后期 `.bashrc` 覆盖**,bash session 内 `echo $FOO` 输出 `B` |
| Win 设了 WSLENV 透传 `FOO=A`,WSL 的 `/etc/environment` 也定义 `FOO=B` | distro 默认 env 也是先于 WSLENV 注入,**实际由 init 顺序决定**;通常 WSLENV 后写入 → 透传变量在 bash 启动前的环境优先,但 `.bashrc` 仍可覆盖 |
| WSL 端 `export WSLENV=X/w`(临时改) | **不持久**,关闭 WSL 后丢失。要持久得改 `.bashrc`/`.profile` |
| 宿主 WSLENV 拼到 Windows PATH 后(`PATH=...;%WSLENV%`) | "WSLENV gets chopped off"(devblog 原文),Windows env 解析会异常,**不要拼接** |
| Win 进程 env 已有 WSLENV,新启动子进程又设了 WSLENV | **后设的覆盖前设的**(标准 CreateProcess env block 行为)。安全做法是**先读旧值再前缀追加**(Windows Terminal 和 wslgit 都这么做) |

**给 mini-term 的实践含义**:用户在 `.bashrc` 里 `export FOO=mine` 会**覆盖** mini-term 通过 WSLENV 注入的 `FOO`。这是 Linux shell 启动顺序决定的,**不是 bug**,但要在 UI 文案说清楚。

---

## 2. 业界类似工具实践

### 2.1 Windows Terminal(微软官方,**最权威参考**)

源码:`microsoft/terminal/src/cascadia/TerminalConnection/ConptyConnection.cpp:66-148`(注释明确标记 `WSLENV.1` ~ `WSLENV.6` 六步)

**做法**:
- profile 里有 `"environment": { "KEY": "value" }`(键值对,**无 flag 概念**)。
- 启动时,Windows Terminal **同时**:
  1. 把每个 KV 写入 CreateProcess 的 lpEnvironment;
  2. 自动把每个 key **以无 flag 形式**追加到 WSLENV(双向透传)。
- **特殊处理:`PATH` 被显式 hardcode 排除**,因为透传 PATH 会污染 WSL 的 Linux PATH 计算(注释原文 "We never want to put a custom Windows PATH variable into WSLENV, because that would override WSL's computation of the NIX PATH")。
- 不区分用户输入 enabled/disabled,全部启用。
- **不做路径自动转换**(用户自己决定要不要写 `/p`)。

**警告/提示**:无;只在 schema description 写 "Environment variable names are not case-sensitive. You can reference existing environment variable names by enclosing them in literal percent characters (e.g. %PATH%)" —— 但这是讲 `%FOO%` 展开,不是 WSL 警告。

### 2.2 JetBrains IDEA / Gateway(WSL toolchain)

源码:`JetBrains/intellij-community/plugins/terminal/src/org/jetbrains/plugins/terminal/util/TerminalEnvironment.kt`

```kotlin
val newItems = envNamesToPass.filter { it != WSLENV }.map { "$it/u" }
val allItems = listOfNotNull(envs[WSLENV]?.removeSuffix(COLON)) + newItems
envs[WSLENV] = allItems.joinToString(COLON)
```

**做法**:
- 把所有用户在 IDE Run Configuration 里配的 env vars + 内置 `TERMINAL_EMULATOR` / `TERM_SESSION_ID` **统一附加 `/u` flag**(仅 Win→WSL,不做翻译)。
- 显式过滤掉用户自定义的 `WSLENV`(防止递归)。
- 保留已存在的 WSLENV value,**追加而非覆盖**。
- `git4idea/GitScriptGenerator.java:58-60` 在 `wsl.exe` 内启动 java 时反过来用 `/w` flag 把 IJ 的 ssh-askpass/git-askpass 端口透传回 Win 侧的 java。

**配置形态**:IDE Run/Debug Configuration → Environment Variables 字段(全局 + 每个配置覆盖),不区分 enabled/disabled。**不做路径自动翻译**(`/u` 不含 `/p`)。

**警告**:无显式警告条,文档只在 KB 提一句 "Environment variables are propagated to WSL via WSLENV"。

### 2.3 VS Code Remote-WSL

源码:`microsoft/vscode/resources/win32/bin/code.sh:38-46`

```sh
if [ $IN_WSL = true ]; then
    export WSLENV="ELECTRON_RUN_AS_NODE/w:$WSLENV"
    ...
```

**做法**:
- VS Code 本身用 WSLENV **反向**(`/w`,WSL→Win)把 `ELECTRON_RUN_AS_NODE` 透给从 WSL 里启动的 Electron 进程。
- **Remote-WSL 的 terminal/debug**:用户在 VS Code settings 的 `terminal.integrated.env.windows` 或 `terminal.integrated.env.linux` 里配 env(`src/vs/workbench/contrib/terminal/common/terminalConfiguration.ts:446-466`),这些 env 会**直接 mixin 到 terminal child process 的 env block**;在 Remote-WSL 模式下,VS Code Server 本身已经跑在 WSL 内,所以 `env.linux` 设的变量天然就在 Linux 进程 env 里,**不走 WSLENV**(因为不再是 Win→WSL 透传场景)。
- **没有专门的"项目级 WSL env vars"UI**,Workspace settings 可以覆盖 user settings,变量级别用户自己 split。

### 2.4 wslgit(典型 Rust WSL wrapper,可作 mini-term 参考)

源码:`andy-5/wslgit/src/wsl.rs::share_val()`

```rust
pub fn share_val(key: &str, value: &str, translate_path: bool) {
    env::set_var(key, value);
    let wslenv_key = if translate_path { format!("{}/p", key) } else { key.to_owned() };
    // 读旧 WSLENV,去重后追加(不重复)
    let wslenv = match env::var("WSLENV") { /* ... */ };
    env::set_var("WSLENV", wslenv);
}
```

**做法**:函数签名直接暴露 `translate_path: bool` 一个布尔,只支持"翻译" / "不翻译"两档,对应 `/p` 和无 flag。**为单变量场景设计**,需要批量时上层循环调用。

### 2.5 Tabby(开源 Electron 终端)

源码:`Eugeny/tabby/tabby-electron/src/shells/wsl.ts`

**做法**:WSL profile 的 `env` 字段只设了 `TERM` / `COLORTERM`,**没有任何 WSLENV 处理**。换言之,Tabby **不支持**把宿主项目级 env 透传到 WSL 内 bash —— 等于 mini-term 当前 v1 的状态。这反而说明 mini-term 想做 v2 的话,是一个**对标 Windows Terminal、领先 Tabby** 的差异化点。

### 2.6 Hyper(Vercel 终端)

GitHub Code Search `WSLENV repo:vercel/hyper` → **0 命中**。Hyper 不处理 WSLENV,WSL profile 与普通 shell 一视同仁。同 Tabby。

### 2.7 wsl.exe / wslconfig 命令行参数侧

`wsl --help` 全量参数中**没有** `--env` / `--env-file` / `--passenv` 一类选项(2026 年最新 1.2.x 仍如此)。`wsl.exe -d <distro> -u <user> --cd <path> -- <command>` 之外,新引入的 `--shell-type {standard|login|none}`(WSL 0.61+)只控制 shell 启动方式,不传环境变量。

**结论**:WSLENV 至今仍是唯一 supported 机制。任何方案都绕不开它。

---

## 3. 边界与陷阱

### 3.1 变量名 / 值合法性

- WSLENV 解析用 `strtok_r(EnvList, ":", ...)` + `strchr(EnvName, '/')`:**名字本身不能含 `:` 或 `/`**,否则会被切断/误识别为 flag 分隔。
- mini-term 前端已校验 key 匹配 `^[A-Za-z_][A-Za-z0-9_]*$` —— 已经规避所有非法字符,**无需再针对 WSLENV 加额外校验**。
- value 中含 `:`:**重要边界**
  - 无 flag / `/u`:value 是普通字符串透传,`:` 不会被切分,**安全**。
  - `/l`:`:` 在 WSL 端就是分隔符,Win→WSL 时 `;` → `:`,如果用户 value 已含 `:`,WSL 端会把它当多个路径。
  - `/p`:单路径模式,value 含 `:` 会触发 Win 端路径合法性失败(`UtilTranslatePathList` 报错),SkipTranslation,**变量被丢弃**(静默)。
- value 含 `\n` / `\r` / `\0`:mini-term 前端已校验拒绝,符合 Windows env block(NUL 是分隔符,会截断)和 POSIX env(NUL 非法)双重要求。

### 3.2 性能与容量

- Windows 进程 env block 总大小上限 **32760 字符(UTF-16 wchar)**,单变量没有独立上限,只受总块限制。
- WSLENV value 本身只列**名字 + flag**,不含变量内容。50 个变量,平均名 12 字符 + `/u` + `:` = 约 16 字符 → 800 字符,远低于上限。
- 实际瓶颈是**单个 value 长**(比如几 KB 的 token)。50 个变量 × 平均 100 字符 value = 5KB,仍安全。
- mini-term 前端没有数量上限校验,**业务上**推荐 modal 显示一个"建议每个项目控制在 30 条以内"的软提示,但不需要硬限制。

### 3.3 WSL 1 vs WSL 2

- WSLENV 解析逻辑在 distro init 里,WSL 1 和 WSL 2 共用同一份 init 代码(`microsoft/WSL` 仓的 `src/linux/init/util.cpp`)。**行为完全一致**。
- 唯一可能差异是路径翻译:WSL 1 文件系统挂在 `/mnt/c/...`,WSL 2 也是 `/mnt/c/...`(`/etc/wsl.conf` 可改前缀),`/p` `/l` 翻译规则相同。
- mini-term 项目 cwd 是 `\\wsl$\<distro>\...` 或 `\\wsl.localhost\<distro>\...` 形式,这两种都对应 WSL 2(WSL 1 的访问方式不同,但 mini-term 已用 `parse_wsl_unc` 统一识别)。**不需要为 WSL 1/2 分支**。

### 3.4 旧版 wsl.exe

- 见 1.1:Tauri v2 最低 Windows 1803 (build 17134) > WSLENV 起步 17063,**无兜底必要**。
- 防御性兜底:如果用户用了魔改 distro / 极旧版 init.exe 不解析 WSLENV,**最坏后果是变量没注入,不会崩溃**。可接受。

### 3.5 mini-term 最低支持平台

- README 标榜 "Windows 主要支持,macOS/Linux experimental"。
- `tauri.conf.json` 无显式 Windows min version 字段(Tauri v2 默认要求 1803+,与 Microsoft Edge WebView2 同要求)。
- `Cargo.toml` 用 `windows` crate v0.58,该 crate 支持 Win10 1809+,与 Tauri v2 一致。
- **结论**:所有支持的目标 Windows 都有 WSLENV,**v2 方案可以无条件启用**。

---

## 4. mini-term 技术选型建议

### 4.1 三方案对比

| 维度 | 方案 A(保守 `/u`) | 方案 B(VS Code/WT 风格,无 flag) | 方案 C(高级:UI 选 flag) |
|---|---|---|---|
| WSLENV 默认 flag | `/u`(仅 Win→WSL,不翻译) | 无 flag(双向透传,不翻译) | 用户每行选 `none` / `/u` / `/p` / `/l` |
| 路径自动转换 | 不做 | 不做 | 用户主动选 `/p` 或 `/l` |
| modal UI 改动量 | 仅改 isWslPath 警告条文案(黄→绿/移除) | 同 A | 每行新增一个 flag 下拉框,需新增 ProjectEnvVar.flag 字段 + 持久化 + 校验 |
| 后端 pty.rs 改动 | 移除 `wsl_override.is_none()` 跳过,拼 `WSLENV=K1/u:K2/u:...` + 逐条 cmd.env | 同 A 但 flag 用空串(`WSLENV=K1:K2:...`) | 同 A 但 flag 来自每条 envVar.flag |
| 用户教育成本 | 低(只解释"不做路径翻译,Win 路径在 WSL 里仍是 Win 路径") | 中(双向透传会反过来:用户在 WSL 改 export FOO 也可能影响 Win,不直观) | 高(modal 内要解释 4 种 flag 的差别;非高级用户看不懂 `/p` vs `/l`) |
| 边界场景报错可解释性 | 高(用户 value 出问题就是 value 本身,与 flag 无关) | 高(同 A) | 中(用户选错 flag 后变量"静默消失",难定位) |
| 与 `.bashrc` 冲突的覆盖语义 | `.bashrc` 仍覆盖(无解,Linux shell 行为) | 同 A | 同 A |

### 4.2 推荐方案:**A(`/u` 默认)**

**理由**:

1. **与 JetBrains 对齐**(`/u` 是业内最权威 IDE 的选择):IDEA terminal 默认 `/u`,意味着"宿主声明,Win→WSL 单向,不翻译"是经过验证的稳定选择。
2. **比方案 B 更保守**:`/u` 阻止 WSL 端反向把变量带回 Win 进程(Windows Terminal 用无 flag 的代价是 WSL 内 `export` 同名变量会被反向带出去,虽然 mini-term 的 PTY 关闭后宿主 env 不持久,但仍然是一种意外通道)。
3. **路径不翻译是合理选择**:mini-term 无法可靠判断用户填的"路径"是 Win 还是 Linux 风格(`C:\foo` 显式是 Win,但 `/home/user/x` 既可能是用户想要的 Linux 路径,也可能是 Win 端 mount 的 cygwin 路径)。**让用户自己决定 value 的形态**比错猜风险低。
4. **modal UI 几乎零改动**:`enabled / key / value` 表单结构完全复用,只需把当前黄色警告条改成绿色提示("WSL 项目环境变量通过 WSLENV 透传,路径请直接填 Linux 风格,如 `/home/u/...`")。
5. **后端改动可控**:在 `pty.rs:670-687` 把 `if wsl_override.is_none()` 分支拆成两路 —— 普通 cwd 走原逻辑;WSL 分支额外构造 `WSLENV=K1/u:K2/u:...` 字符串(过滤 MINITERM_ 前缀),与 cmd.env 一起注入。**约 30 行代码**。
6. **MINITERM_* 防御性过滤同样适用**:WSL 分支拼 WSLENV 时也要跳过 MINITERM_ 前缀,保持与普通 cwd 分支一致的契约。

### 4.3 推荐 Open Questions 一并决策

回到 `prd.md` 的 Open Questions:

1. **Q1 (WSLENV flag 默认)** → `/u`(方案 A)。
2. **Q2 (路径自动转换)** → 不做(方案 A 不含 `/p` `/l`)。后续可作为"高级勾选"加在 modal 里,但 v2 范围外。
3. **Q3 (用户自定义 WSLENV 冲突)** → 用户填的 `WSLENV` key **加入 MINITERM_ 同级别的保护**,前端 reject + 后端跳过(避免覆盖 mini-term 拼出来的 `WSLENV=K1/u:K2/u:...`)。这条建议加进前端校验。
4. **Q4 (警告条改写)** → 改成**绿色提示条**:"已通过 WSLENV 透传至 WSL bash(`/u` 单向,不做路径翻译)。值是路径时请填 Linux 风格;`.bashrc` 中 `export` 同名变量会覆盖此值"。
5. **Q5 (MINITERM_* hook 协议透传到 WSL)** → 显式 **Out of Scope**(同 prd.md 已表态),理由:WSL VM 内 process_monitor 看不到子进程,即使透传 PTY_ID 也无法关联,需要先做"WSL 内 AI 进程识别"才有意义,本任务不解决。

### 4.4 拼接示例(伪代码)

```rust
// pty.rs 改写示意
let user_envs: Vec<(String,String)> = envs.unwrap_or_default()
    .into_iter()
    .filter(|(k,_)| !k.starts_with("MINITERM_") && k != "WSLENV")
    .collect();

if wsl_override.is_some() {
    // 拼 WSLENV value(/u flag),保留宿主已有 WSLENV(防御覆盖)
    let mut parts: Vec<String> = user_envs.iter()
        .map(|(k,_)| format!("{}/u", k))
        .collect();
    if let Ok(existing) = std::env::var("WSLENV") {
        if !existing.is_empty() {
            parts.push(existing);
        }
    }
    let wslenv_value = parts.join(":");
    cmd.env("WSLENV", wslenv_value);
}

// 同时注入实际 KV(WSL 分支也要走这步,与 v1 不同)
for (k, v) in &user_envs {
    cmd.env(k, v);
}
```

### 4.5 验证计划补充

prd.md 的 Acceptance Criteria 基础上,**新增 3 条**:

- AC-NEW-1: 宿主 PowerShell 已 `$env:WSLENV="FOO/u"`(用户既有 WSLENV),mini-term 启动 WSL 终端,WSL 内 `echo $WSLENV` 看到 `mini-term 拼接的 K1/u:K2/u:FOO/u`(已合并未覆盖)。
- AC-NEW-2: WSL `.bashrc` 末尾 `export FOO=from_bashrc`,项目设 `FOO=from_mini-term`,WSL 内 `echo $FOO` 输出 `from_bashrc`(确认 `.bashrc` 覆盖的预期行为,UI 文案需对此说明)。
- AC-NEW-3: 项目设 `FOO=val:with:colons`(含 `/u` 默认 flag),WSL 内 `echo $FOO` 输出完整字符串 `val:with:colons`(确认 `:` 在 value 中不会被 WSLENV 解析切分)。

---

## 5. 引用来源

### Microsoft 官方

- [WSLENV announcement (Microsoft devblog, 2018)](https://devblogs.microsoft.com/commandline/share-environment-vars-between-wsl-and-windows/) — Craig Wilhite, PM
- [Working across file systems (Microsoft Learn)](https://learn.microsoft.com/en-us/windows/wsl/filesystems) — WSLENV flags 官方表格
- [WSL basic commands (Microsoft Learn)](https://learn.microsoft.com/en-us/windows/wsl/basic-commands) — `wsl.exe` 全量 CLI 参数(确认无 `--env`)
- `microsoft/WSL/src/linux/init/util.cpp::UtilParseWslEnv` (lines 2110-2265) — distro 启动时 WSLENV 解析权威实现
- `microsoft/terminal/src/cascadia/TerminalConnection/ConptyConnection.cpp:55-148` — Windows Terminal 的 WSLENV 注入逻辑(`WSLENV.1` ~ `WSLENV.6` 6 步注释)
- `microsoft/terminal/doc/cascadia/profiles.schema.json` — `profile.environment` 字段定义

### JetBrains

- `JetBrains/intellij-community/plugins/terminal/src/org/jetbrains/plugins/terminal/util/TerminalEnvironment.kt` — `/u` flag 默认策略
- `JetBrains/intellij-community/plugins/git4idea/src/git4idea/commands/GitScriptGenerator.java:40-65` — `/w` flag 反向用例
- `JetBrains/intellij-community/platform/platform-impl/src/com/intellij/openapi/vfs/impl/wsl/WslConstants.java` — `WSLENV` 常量定义

### VS Code

- `microsoft/vscode/resources/win32/bin/code.sh:24-46` — `WSLENV="ELECTRON_RUN_AS_NODE/w:$WSLENV"` 标准追加模式
- `microsoft/vscode/src/vs/workbench/contrib/terminal/common/terminalConfiguration.ts:446-473` — `terminal.integrated.env.{windows,linux,osx}` 配置 schema

### 第三方参考实现

- `andy-5/wslgit/src/wsl.rs::share_val()` — Rust WSLENV 拼接函数,可作 mini-term 后端 reference
- `Eugeny/tabby/tabby-electron/src/shells/wsl.ts` — Tabby 不处理 WSLENV(反例,说明 mini-term v2 是差异化点)

### mini-term 内部

- `src-tauri/src/pty.rs:599-687` — 当前 v1 实现与 WSL 跳过注释
- `.trellis/spec/backend/pty-env-vars-injection.md` — 现有契约(含 v2 预留位置 302 行)
- `src-tauri/mt-core/src/wsl_path.rs::parse_wsl_unc` — WSL UNC 识别规则
- `src/components/ProjectEnvVarsModal.tsx:76-204` — 现有警告条 UI(本任务需替换)

# 同类工具如何让用户"在 WSL 目录里开终端"

- **Query**: 调研 Windows Terminal / VS Code Remote-WSL / JetBrains / Tabby / Wezterm / Alacritty / Hyper / Warp / GitHub Desktop 等工具的 WSL 入口、命令、路径展示、文件操作方式，给 mini-term 写"WSL 项目根"功能时找一组可遵循的最小约定
- **Scope**: 外部
- **Date**: 2026-05-25

---

## TL;DR (≤200 字)

主流终端的共同做法是：**自动**枚举已安装 distro，**每个 distro 生成一个 profile**（名字像 "WSL: Ubuntu"），命令固定为 `wsl.exe -d <DistroName>`，工作目录字段写 UNC（`\\wsl$\<Distro>\home\<user>\proj` 或 `\\wsl.localhost\…`），文件操作直接走 Windows 9P bridge。VS Code / JetBrains 走另一条路——把整个 IDE 拆成宿主 UI + WSL 内 server，对用户暴露的是 Linux 视角的 `/home/user/proj`。**对 mini-term 这种纯终端管理器来说，Windows Terminal / Tabby / Wezterm 的"distro + UNC 启动目录"路线最合适**：在"项目路径"基础上加一个可选 `distroName`，启动命令拼 `wsl.exe -d <distro> --cd <linux-path>`，文件树仍走宿主 9P。9P 慢 10-20×、`fs.watch` 对 UNC 支持差需提前预知。

---

## 1. Windows Terminal

**来源**:
- https://learn.microsoft.com/en-us/windows/terminal/dynamic-profiles
- https://learn.microsoft.com/en-us/windows/terminal/customize-settings/profile-general

### 入口

- **自动检测**：WT 启动时自动为每个已安装的 WSL distro **动态生成 profile**，profile source 是 `Windows.Terminal.Wsl`。装新 distro 后下次启动自动加（无需用户配置）。
- **手动**：用户也可以从 explorer 浏览到 `\\wsl$\` 目录后右键 "Open in Windows Terminal" —— 但这是 explorer 提供的（基于 [`HKCR\Directory\shell\WT`](https://learn.microsoft.com/en-us/windows/terminal/install#install-the-windows-terminal)），WT 启动时把当前 explorer 路径作为 `startingDirectory`。

### 命令与启动目录

文档明确给出的 settings.json 字段：

```json
{
  "name": "Ubuntu-20.04",
  "source": "Windows.Terminal.Wsl",
  "commandline": "wsl.exe -d Ubuntu-20.04",
  "startingDirectory": "\\\\wsl$\\Ubuntu-20.04\\home\\user1"
}
```

或在 settings UI 中以 `//wsl.localhost/Ubuntu-20.04/home/user1` 形式（forward-slash + `wsl.localhost`）填写。后端启动时把 UNC 转成 Linux 路径传给 `wsl.exe`（实际效果等于在 distro 里 `cd /home/user1` 后开 shell）。

### 路径展示

- 用户填写时是 UNC（`\\wsl$\…` 或 `\\wsl.localhost\…`）
- 标签页显示 distro 名 / shell 名
- 当前工作目录在终端内由 shell 自己显示（Linux 路径 `/home/user/proj`）

### 关键启示

WT 没有"项目"概念，但它的 UNC + `wsl.exe -d` 组合是事实标准。所有 Windows 应用想给 WSL 开终端都可直接套用这套字段。

---

## 2. VS Code Remote-WSL

**来源**:
- https://code.visualstudio.com/docs/remote/wsl
- https://raw.githubusercontent.com/microsoft/vscode-docs/main/docs/remote/wsl.md

### 入口（**注意：这是和别的工具最大的不同**）

VS Code 走"双进程拆分"路线，不是单纯的终端启动器：

1. **从 WSL 终端进**：用户在 WSL 内 `cd /home/u/proj && code .`，本机 VS Code 自动启动并把项目当作 WSL workspace 打开
2. **从 VS Code 进**：F1 → `WSL: Connect to WSL` 或 `WSL: Connect to WSL using Distro`（多 distro 时弹列表选）
3. **从 Windows CMD**：`code --remote wsl+Ubuntu /home/jim/proj` 或 `--folder-uri vscode-remote://wsl+Ubuntu/home/u/proj`

### 架构（决定了文件树和终端的行为）

```
Windows side: VS Code UI / 主进程
              ↕ JSON-RPC over local TCP
WSL side: VS Code Server（自动下载到 ~/.vscode-server，与 Windows 端独立）
          ↕ 直接读 Linux fs / 起 PTY
```

**文件树**：由 server 在 Linux 内枚举 `/home/u/proj`，通过 RPC 推给 UI 渲染。所以不走 9P bridge，**速度等同于本地**。

**Integrated terminal**：一旦 workspace 已是 remote-wsl 模式，新开 terminal 自动调用 server 端 `posix_spawn`，PTY 在 WSL 内。Windows 端 xterm.js 只是显示。

### 路径展示

- 状态栏左下角显示 `WSL: Ubuntu`（distro 名）
- 资源管理器显示 Linux 视角（`/home/u/proj/src/...`）
- 状态栏 + workspace title 双标识

### 如果用户走 UNC 路径直接打开（不连 remote-wsl）

> 文档语气："如果 Git 没有装在 Windows 上，VS Code 会去 WSL 里找；当工作区路径以 `\\wsl$` 开头，VS Code 自动切到 WSL 的 git"

但这是 fallback，**性能很差**——一切走 9P bridge，文件 watcher 在 WSL1 还有 EACCES 问题（`remote.WSL.fileWatcher.polling` 设 true 才能绕过）。文档明确建议走 remote-wsl 模式。

### 关键启示

VS Code 这条 server-in-WSL 路线**对纯终端管理器太重**（要在 distro 里塞一个 daemon）。但它的 UI 暴露方式可参考：状态栏永远显示 distro 名 + Linux 路径，让用户感觉"在 WSL 项目里"而不是"在 UNC 路径里"。

---

## 3. JetBrains IDEs (IntelliJ / PyCharm / WebStorm / GoLand …)

**来源**:
- https://www.jetbrains.com/help/idea/how-to-use-wsl-development-environment-in-product.html
- https://www.jetbrains.com/help/idea/settings-tools-terminal.html

### 入口

- **新建项目**：New Project 对话框，Location 字段填 `\\wsl.localhost\Ubuntu\home\test\MyProject`（UNC，明确写在文档示例里）
- **打开已有项目**：File → Open，输入 `\\wsl.localhost\<DistributionName>\…`
- **没有"WSL Toolbox"概念**（这是用户在搜索时常见的误名）；JB 把 WSL 当作"远程 target"的一种，叫 "Run Targets" 或 "WSL"
- **Run Targets**：项目在 Windows fs 时，可以让运行/调试在 WSL 内执行（New Target → WSL）

### 命令与工作机制

JB IDE **本身仍在 Windows 跑**，但 SDK/runtime/git 都让 WSL 内的 binary 来执行：

- 用 `wsl --exec ...` 调 Linux 进程
- File watcher 用 polling
- 调试器走 vEthernet (WSL) 网络

**Integrated Terminal**：打开 WSL 项目时，Terminal 选项卡默认使用 WSL 的 shell，相当于 `wsl.exe -d <distro>`（受 Settings > Tools > Terminal 中 Shell path 影响，可以指向 `wsl.exe` 或具体 `wsl.exe --distribution Ubuntu`）

### 路径展示

- 项目根、文件树都是 UNC 形式 `\\wsl.localhost\Ubuntu\home\…`
- IDE 顶部窗口标题里显示 `…\MyProject [WSL]`
- Git 集成自动用 WSL 内 git（注释里写："IntelliJ automatically switches to Git from WSL for projects opened with `\\wsl$` path"）

### 关键启示

JB 的方案介于 WT 和 VS Code 之间：
- **不像 VS Code** 那样塞 server，整个 IDE 在宿主
- **像 VS Code** 那样把 distro 当一等公民（特殊 indicator、自动切 git 后端）
- 路径显示就用 UNC 原文，不做翻译

mini-term 完全可以借鉴："项目路径是 UNC，但显示一个 WSL 徽章 + distro 名"

---

## 4. Tabby

**来源**: 直接看源码 https://raw.githubusercontent.com/Eugeny/tabby/master/tabby-electron/src/shells/wsl.ts

### 实现细节（最有参考价值的一个）

Tabby 启动时调用 `WSLShellProvider.provide()`:

1. **读注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`** 枚举所有 distro
2. 对每个 distro 创建一个 `Shell` 对象：
   ```ts
   {
     id: `wsl-${slug}`,
     name: `WSL / ${name}`,            // 例: "WSL / Ubuntu-22.04"
     command: `${windir}\\system32\\wsl.exe`,
     args: ['-d', name],
     fsBase: `\\\\wsl$\\${name}`,      // WSL2: UNC base, WSL1: childKey.BasePath\rootfs
     env: { TERM: 'xterm-color', COLORTERM: 'truecolor' },
     shellType: 'unix',
     icon: distroIconMap[name] ?? linuxIcon,
   }
   ```
3. WSL1 vs WSL2 通过 `childKey.Flags & 8` 区分（位 8 = WSL2）
4. Default distro 同时单独生成一个 "WSL / Default distro" profile，命令直接是裸 `wsl.exe`（不带 `-d`）
5. **图标按发行版查表**（Ubuntu/Debian/Alpine/Kali/openSUSE/…）

### 路径展示

- profile 名 `WSL / Ubuntu`，下拉菜单图标是发行版 logo
- `fsBase` 字段存的是 UNC（`\\wsl$\Ubuntu`），用于 file uploader / sftp / 显示当前路径
- 不存 Linux 路径，所有展示都用 UNC

### 没有"项目"概念

Tabby 没有 project root 的抽象。但 plugin 系统里有 `tabby-workspace-manager` 插件提供 workspace profile，本质上就是一组 `{profile, startingDirectory}`。

### 关键启示

**Tabby 的代码是 mini-term 可以直接照抄的模板**：
- 注册表 `Lxss` key 枚举 distro
- `wsl.exe -d <name>` 启动
- UNC 作为 fsBase
- WSL1/WSL2 用 Flags & 8 区分（rootfs 路径不同）

---

## 5. WezTerm

**来源**: https://wezterm.org/config/lua/WslDomain.html, https://wezterm.org/config/lua/config/wsl_domains.html

### 概念："WslDomain"

WezTerm 把 WSL 抽象为一种 multiplex domain：

```lua
config.wsl_domains = {
  {
    name = 'WSL:Ubuntu-18.04',            -- 在 launcher menu 显示的名字
    distribution = 'Ubuntu-18.04',         -- 必须匹配 wsl -l -v 输出
    username = 'hunter',                   -- 可选；默认用 distro 默认用户
    default_cwd = '/tmp',                  -- 可选；不指定时继承
    default_prog = {'fish'},               -- 可选 shell override
  },
}
```

### 自动检测

`wezterm.default_wsl_domains()` 解析 `wsl -l -v` 输出，**默认就是它**——用户什么都不写也能看到所有 distro 作为 launcher 选项。

### 命令与路径

底层调 `wsl.exe -d <dist>`，但语义包装为 "domain"——同一 domain 里新开 tab/pane 会**继承当前 working directory**。

### 关键启示

WezTerm 的 "domain" 抽象比 WT/Tabby 更上层：
- distro 不是 profile 的一个字段，而是独立维度
- 允许 user / default_cwd / default_prog 分别覆盖
- 适合 mini-term 想要的"项目 = (UNC path, distro)"思路

---

## 6. Alacritty

**来源**: https://raw.githubusercontent.com/alacritty/alacritty/master/extra/man/alacritty.5.scd

Alacritty 是极简终端，**完全没有 WSL 专属支持**：
- 只有一个 `working_directory` 字段
- shell 通过 `[shell]` table 配置 `program = "wsl.exe"`, `args = ["-d", "Ubuntu"]`
- 用户自己写 `~/.config/alacritty/alacritty.toml`

实际上 Windows 用户的 Alacritty 配置就是 hand-rolled 一份 `wsl.exe -d Ubuntu` + `working_directory = "//wsl$/Ubuntu/home/u/proj"`。

### 关键启示

证明"profile 字段三件套：command + args + working_directory"足以表达 WSL 项目根，**不需要新建专门的数据结构**。

---

## 7. Hyper

**来源**: https://hyper.is/ 配置文档（`~/.hyper.js`）

Hyper 也没有内置 WSL profile/枚举。用户手动配置：

```js
shell: 'C:\\Windows\\System32\\wsl.exe',
shellArgs: ['-d', 'Ubuntu'],
cwd: '\\\\wsl$\\Ubuntu\\home\\user\\proj',
```

没有任何 distro 概念或自动检测。生态里有第三方 plugin (`hyper-wsl-tabs`) 试图补这个能力，但社区采纳很弱。

### 关键启示

完全 hand-roll 路线——mini-term 如果只能选一个 MVP，至少要做到 Hyper 的水平：让用户能手填 UNC + `wsl.exe -d` 命令。

---

## 8. Warp

**来源**:
- https://www.warp.dev/blog/launching-warp-on-windows (2025-02-26)
- https://docs.warp.dev/getting-started/quickstart/installation-and-setup.md
- https://docs.warp.dev/llms-small.txt

### 现状

- Warp Windows 版本于 2025-02-26 公开发布
- 官方明确支持的 shell：**"PowerShell, WSL, Git Bash"**
- 但**官方公开文档对 WSL 的 UX 描述很少**。Settings → Features → Session → "Startup shell for new sessions" 中可以选 WSL

### Codebase Context 限制（次要发现）

Warp 的 AI Codebase Context (semantic indexing) **不支持 WSL session**（GitHub #6744）——说明 Warp 内部把 WSL session 当作 SSH 一样的远程会话来对待。

### 关键启示

- Warp 把 WSL 当作 first-class shell（与 PowerShell / Git Bash 并列）
- 但是其 codebase indexing 等高级功能在 WSL 中不可用，说明跨 9P 做大量文件操作确实会被绕开
- mini-term 也应该接受："WSL 项目里有些功能（如 .gitignore filter / 文件 watcher）需要降级"

---

## 9. GitHub Desktop / Sourcetree

**来源**:
- https://github.com/desktop/desktop/issues/22044 ("WSL repos are 10-20x slower than native Windows repos due to 9P filesystem boundary")
- https://github.com/desktop/desktop/issues/9443 (x-bit confusion)
- https://github.com/desktop/desktop/issues/14498 (Script executes from Windows not WSL)

### GitHub Desktop 的对待 UNC 路径

- **能用**——可以在 "Add Local Repository" 时填 `\\wsl$\Ubuntu\home\u\proj`，UI 不会拒绝
- **但很慢**——所有 git 操作走 Windows `git.exe` + 9P bridge，慢 10-20×
- **小坑**：x-bit / chmod / 路径大小写在 UNC 路径下会被误判为修改
- 默认 hook 由 Windows 端执行（不是 WSL 内），脚本可能挂

### 路径显示

GitHub Desktop UI 里的 repo path 就用 UNC 字符串原文显示（不做美化、不显示 distro 标）。

### Sourcetree

Atlassian 官方没有为 WSL 做任何特殊集成；用户社区里讨论的方案就是手填 UNC 路径，行为和 GitHub Desktop 类似。

### 关键启示

- UNC 路径作为标识在 GUI 工具里"能用但难看"——mini-term 应该做 UI 美化
- "宿主进程跨 9P 操作 WSL 文件"是合法的但有性能/正确性代价，要提前对用户做提示

---

## 10. WSL 本身提供的工具栈（事实约定）

**来源**:
- https://learn.microsoft.com/en-us/windows/wsl/basic-commands
- https://learn.microsoft.com/en-us/windows/wsl/filesystems

### 必备命令行知识

| 命令 | 输出/作用 |
|---|---|
| `wsl -l -v` (`wsl --list --verbose`) | distro 列表 + WSL1/WSL2 + 运行状态 |
| `wsl -l -q` | 只输出 distro 名（脚本用） |
| `wsl --status` | 默认 distro / kernel 版本 |
| `wsl -d <name> [--cd <linux-path>] [--user <u>] [-- <cmd>]` | 在指定 distro 内执行 |
| `wsl ~` | 默认 distro，cwd = 用户 home |

### `--cd` 是关键 flag

`wsl.exe --cd <path>` 直接指定 Linux 工作目录（路径可以是 Linux 形式或 Windows UNC），比传统的"在终端里再 `cd`"更干净。**这是 mini-term 应该用的启动方式**：

```
wsl.exe -d Ubuntu --cd /home/user/proj
```

### UNC 路径形式

- `\\wsl$\<Distro>\path` (老形式，仍支持)
- `\\wsl.localhost\<Distro>\path` (新形式，2021 起的 Win10/11)
- Linux 形式：`/home/user/proj`（在 distro 内）
- 两者可双向互转：`wslpath` 工具

### 9P 性能/语义注意

- 文档明确建议："如果在 Linux 命令行工作，把文件放在 WSL fs；如果在 Windows 命令行工作，把文件放在 Windows fs"
- 跨边界（Windows 应用读 WSL fs）每次 IO 都过 9P VM boundary
- **`fs.watch` 在 UNC / 9P 上不可靠**（VS Code 文档里专门提到要切 polling）

---

## 11. 横向结论

### 入口约定

| 工具 | 自动枚举 distro | 单独 distro 选择器 | 用户手填 UNC |
|---|---|---|---|
| Windows Terminal | ✅ 动态 profile | ❌（用 profile name） | settings.json 可填 |
| VS Code Remote-WSL | ✅ Command Palette 列表 | ✅ "Connect using Distro" | ✅ 直接 Open Folder |
| JetBrains | ❌ | ❌ | ✅ 全靠 UNC |
| Tabby | ✅ 注册表枚举 | ❌（每个 distro 一个 profile） | profile cwd 字段 |
| WezTerm | ✅ `wsl -l -v` 解析 | ✅ launcher menu | ❌（domain 抽象） |
| Alacritty/Hyper | ❌ | ❌ | ✅ 手填 |
| Warp | ✅（shell 选项里有 WSL） | ? | ? |
| GitHub Desktop | ❌ | ❌ | ✅ 路径输入框 |

**多数有正经支持的工具都做自动枚举，distro 是显式存在的一等概念。**

### 路径显示

| 工具 | 给用户看的形式 |
|---|---|
| WT, Tabby, JetBrains, GitHub Desktop | UNC 原文 (`\\wsl$\Ubuntu\home\u\proj`) |
| VS Code Remote-WSL | Linux (`/home/u/proj`) + 状态栏 distro 徽章 |
| WezTerm | 在 domain 内看 Linux 路径 |
| Warp | shell 提示符自带，UI 不专门美化 |

**纯终端管理器普遍用 UNC，因为反正终端内显示的是 shell 的提示符 = Linux 路径。**

### 终端启动命令

**100% 都用 `wsl.exe`，区别只在参数细节**：

| 方案 | 命令样例 |
|---|---|
| 最简 | `wsl.exe` (默认 distro) |
| 指定 distro | `wsl.exe -d Ubuntu` |
| 指定目录 | `wsl.exe -d Ubuntu --cd /home/u/proj` |
| 指定用户 | `wsl.exe -d Ubuntu -u alice --cd /home/alice/proj` |
| 老路线（不推荐） | `\Windows\System32\bash.exe`（仅默认 distro，已过时） |

**没有任何主流方案是"启动 powershell → 再 `wsl.exe -d`"或"启动 cmd → 再 `wsl`"这种二段式**。

### 文件操作

| 方式 | 工具 |
|---|---|
| 走 9P / UNC（宿主直接读 Linux fs） | WT, Tabby, JetBrains, GitHub Desktop, Alacritty, Hyper, Warp |
| 在 WSL 内跑 server，RPC 推 file tree | VS Code Remote-WSL（重） |
| 远程执行 `wsl.exe ls` | 没有主流方案这么做 |

**结论**：纯终端管理器普遍直接用宿主的 fs API + UNC 路径。代价是 9P 慢、watcher 不靠谱。

### 是否需要 distro 概念

- 如果支持 0 或 1 个 distro → 不需要，沿用现有的"项目路径"字段加一个 `wsl.exe` shell 就够
- 如果支持多 distro / 项目同名不同 distro / 想正确显示 distro 标 → **需要单独存 `distroName`**
- WT/JB/GitHub Desktop 不存 distro：UNC 路径里就含 distro 名，可解析。但**展示**时仍需要从路径里 split 出来，等于变相还是有 distro 概念

---

## 对 mini-term 的启示（建议落地的设计决定）

> 以下是 mini-term 在写代码前应当作为基线接受的约定。

1. **数据模型 = 现有 `path` 字段 + 新加一个可选 `distroName`（string）**
   - 当 `path` 以 `\\wsl$\` 或 `\\wsl.localhost\` 开头时，**强制** parse 出 distro name 写回 `distroName`（避免和路径不一致）
   - 当 `distroName` 非空时，UI 显示 WSL 徽章 + distro 名（仿 JB、VS Code 状态栏）
   - 这样 SplitNode / Tab 的现有逻辑不变，只是在终端启动那一步多一层判断

2. **入口至少两条**（MVP 可只做 1 + 2，第 3 条是优化）
   1. 用户在新建项目对话框可手填 UNC 路径 `\\wsl$\<Distro>\<path>`（最低保底）
   2. 提供"添加 WSL 项目"按钮，先选 distro（下拉自动列出 `wsl -l -q` 结果），再选 distro 内的子目录（用宿主 fs 浏览 `\\wsl$\<Distro>\`）
   3. （未来）右键 explorer 集成 / `mini-term --add-wsl-project ubuntu:/home/u/proj` CLI

3. **distro 枚举：调 `wsl.exe -l -v`，不读注册表**
   - Tabby 读 `HKCU\…\Lxss` 是历史方案，今天 `wsl.exe -l -v` 完全够用且 forward-compatible
   - 输出是 UTF-16 LE，需要 `decode('utf-16-le')` 后 parse
   - 失败时（用户没装 WSL）静默返回空数组，不报错

4. **终端启动命令固定为 `wsl.exe -d <distro> --cd <linux-path>`**
   - 不用 bash.exe；不用先开 powershell 再 cd
   - `--cd` 接受 Windows UNC 也接受 Linux 路径——为了和现有 `path` 字段对齐，**直接传 UNC**（让 wsl.exe 自己转换）
   - 现有 shell 列表里加一个特殊 entry `"WSL"`，type='wsl'，commandTemplate 包含 `{distro}` 占位

5. **文件树仍走宿主 fs.rs（不要在 WSL 里塞 daemon）**
   - 沿用 `list_directory` + `.gitignore` 过滤；UNC 路径 Rust `std::fs` 直接支持
   - **`notify` watcher 对 UNC 不可靠** → 在 fs.rs 里检测 path 是 UNC 时改用 polling（参考 VS Code 的 `remote.WSL.fileWatcher.polling`）
   - UI 上标注 "WSL 文件操作可能较慢（通过 9P 桥接）"

6. **路径显示策略**
   - 内部存储：UNC 原文（`\\wsl$\Ubuntu\home\u\proj`）
   - UI 短显示：`Ubuntu: /home/u/proj`（解析 UNC，去掉 `\\wsl$\<distro>\` 前缀，斜杠翻转）+ 一个 Ubuntu logo
   - tooltip / 复制路径：UNC 原文

7. **接受的能力降级**
   - `fs.watch` 改 polling（仅 UNC 路径，默认 polling 间隔 2-5s）
   - AI session 历史扫描如果跨 WSL 边界扫描慢，考虑限定到几个常用目录
   - 跨平台 PTY、`process_monitor.rs` 进程名识别不受影响（`wsl.exe` 是宿主进程，其 child claude/codex 通过 conhost 进程不可见——这是 mini-term 在 WSL 项目里识别 AI 进程的潜在难题，需要单独 spike，**不属于本调研范围**）

8. **不要做的事**
   - 不要做 VS Code Remote 那种 server in WSL 模式（投入巨大、超出 mini-term 定位）
   - 不要尝试在终端启动后再发 `cd /home/u/proj`（会和用户 shell rc 抢，rc 里可能 `cd ~`）
   - 不要做"自动转 Linux 路径"再让用户编辑——保留 UNC 原样，转译只在显示层

---

## Caveats / Not Found

- **JetBrains "WSL Toolbox"**：用户问题里提到的术语，实际 JB 文档里**没有**这个名字，正确叫法是 "WSL (Run Targets)"，本文按真实命名记录。
- **Warp 关于 WSL 的 UI 细节**：Warp 公开文档只在 changelog/blog 提到支持 "WSL shell"，对"如何把 WSL 目录作为项目根"几乎没有专门文档。本文只能从"WSL 是 supported shell"+"Codebase Context 不支持 WSL"推断出他们的处理方式接近 "WSL = 一种 shell"，不是 "WSL = project type"。
- **Tabby 的 workspace plugin**：第三方 `tabby-workspace-manager` 提供 workspace/profile 绑定，但文档不全，仅作背景信息列出。
- **`wsl --list -v` 输出编码**：实测 wsl.exe 的输出编码在 Windows 10/11 各版本有差异（UTF-16 LE / UTF-8 with BOM），落地时建议探测；本文未给出最终判断。
- **mini-term 自身代码**：按 task 要求未读，所以"对 mini-term 的启示"是基于现有 CLAUDE.md / store/PTY 概览推断的，落地时需 implement agent 二次校验。

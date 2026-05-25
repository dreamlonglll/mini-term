# Research: CreateProcess / ConPTY 启动新进程时 UNC working directory 的精确行为与绕过方案

- **Query**: 调研 Windows 上 CreateProcess / ConPTY 启动新进程时 "working directory 不能是 UNC" 这条历史限制的细节,以及业界绕过方案
- **Scope**: 混合(internal: portable-pty 源码 + mini-term 现状;external: MSDN, Windows Terminal 源码, VS Code 源码, WSL 文档, 业界 PR 实践)
- **Date**: 2026-05-25
- **Author**: research agent

---

## TL;DR — 初步判断

**核心结论**:`CreateProcessW` API 本身并不拒绝 UNC `lpCurrentDirectory`,真正的限制来自被启动的 shell(尤其是 `cmd.exe`),它在启动后检测到 cwd 是 UNC 时会**打印警告并 fallback 到 `C:\Windows`**,导致用户感受到的"WSL 路径开不了终端"。其他 shell(`powershell`、`pwsh`、`bash via Git`)行为不同,但都不能像 native Linux 进程那样在 UNC 上正常工作。

**为 mini-term 选哪个方案最合理**:**方案 B(识别 `\\wsl$\` UNC → 用 `wsl.exe --cd` 启动)**,理由:

1. **官方权威已有 API**(`wsl.exe --cd <linux-path>`),专门为这场景设计,Win10 1903+ 全版本可用。
2. **行业一致采用**:Windows Terminal、VS Code、wezterm 都采用同样思路(代码已贴在下文)。
3. **绕过 cmd UNC 限制最干净**:从源头不让 `lpCurrentDirectory` 落到 UNC,而是把 cwd 落在 Windows 端安全位置(`%USERPROFILE%` / `C:\`),把"进哪个 WSL 目录"的责任交给 `wsl.exe` 子进程内部 `chdir` 到 Linux 路径。
4. **保留 fallback**:对非 WSL UNC(如 `\\server\share\...`)、对用户明确填了非 wsl shell 的 case,仍按原路径走(可能触发 cmd 警告,但用户行为可控)。

**方案 A(`pushd` / 临时驱动器映射)** 已被 Windows Terminal 维护者 DHowett 明确否决:"I'm wary of us touching global system state—even for the duration of a single process—to work around a fairly narrow issue in a shell we're not intending on doing any new work on. I wouldn't ship that."(microsoft/terminal#19514 comment)

---

## 一、CreateProcessW 对 UNC `lpCurrentDirectory` 的精确行为

### 1.1 MSDN 官方原文

来源:https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw

> **lpCurrentDirectory**
> The full path to the current directory for the process.
> **The string can also specify a UNC path.**
> If this parameter is NULL, the new process will have the same current drive and directory as the calling process.

**结论**:`CreateProcessW` 在 API 文档级别**明确允许** UNC 路径作为 `lpCurrentDirectory`。API 调用本身会成功返回。

### 1.2 真实表现:cmd.exe / pwsh / bash 各自的行为差异

#### cmd.exe(包括 ConPTY 启动 cmd.exe)

CMD.EXE 在启动后会检测当前目录,若为 UNC 路径,**打印警告并 fallback 到 `C:\Windows`**:

```
'\\wsl.localhost\Ubuntu-24.04\home\nagayasu\work\autoadb'
CMD.EXE was started with the above path as the current directory.
UNC paths are not supported. Defaulting to Windows directory.
```

来源:rom1v/autoadb#33(实测复现,带 before/after 输出),vibeyard PR #113。

这是 **cmd.exe 自己的限制**,不是 CreateProcess 的限制。CreateProcess 调用成功,但 cmd 在初始化时主动放弃 UNC cwd。

#### powershell.exe / pwsh.exe

PowerShell 在 UNC 当前目录下能启动,但**不能用 cmd-style 相对路径**,内置 `cd` / `Set-Location` 必须用绝对 UNC 路径。一些命令(尤其调用旧 Win32 API 的)会报错。在 mini-term 场景下 pwsh 会启动成功,但用户每次 `cd ..` 之类的操作要看运气。

来源:经验性证据,加上 anthropics/claude-code#61460 "`code .\folder` not working from powershell if working dir path is UNC"。

#### bash via Git for Windows (MSYS2)

bash 本身没限制,但 Git for Windows 的 MSYS2 路径转换层在 UNC 下不稳定。`pwd` 会显示 `//wsl.localhost/Ubuntu/home/user/proj`(被 MSYS 翻译成 POSIX 风格),git 操作通常能继续,但通过 9P 协议访问性能极差(微软自己警告,见 https://learn.microsoft.com/windows/wsl/filesystems#file-storage-and-performance-across-file-systems)。

#### `\\?\UNC\wsl$\Ubuntu\home\user` verbatim 形式

verbatim 前缀(`\\?\`)告诉 Win32 API "跳过路径解析",在 `CreateFile`/`CreateProcess` 等 API 上**允许 cwd 超过 MAX_PATH**;但它不会改变 cmd.exe 的行为 — cmd 看到 cwd 是 UNC 仍然 fallback。

来源:https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file
> "For file I/O, the `\\?\` prefix to a path string tells the Windows APIs to disable all string parsing and to send the string that follows it straight to the file system."

### 1.3 历史 KB 文档

旧的 KB "CMD does not support UNC paths as current directories"(原 URL: https://learn.microsoft.com/en-us/troubleshoot/windows-client/setup-upgrade-and-drivers/cmd-prompt-doesn-t-support-unc-name-as-current-directory)**当前已 404**(2026-05-25 实测),但这条限制至今未变 — autoadb#33 的 issue 是 WSL Ubuntu 24.04 环境下的最新复现,行为完全一致。

---

## 二、portable-pty 在 Windows 上的实现路径

### 2.1 强制走 ConPTY,无 winpty fallback

源码位置: `C:\Users\12197\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\portable-pty-0.8.1\src\win\psuedocon.rs:44-49`

```rust
fn load_conpty() -> ConPtyFuncs {
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );
    if let Ok(sideloaded) = ConPtyFuncs::open(Path::new("conpty.dll")) {
        sideloaded
    } else {
        kernel
    }
}
```

**关键事实**:
- portable-pty 0.8.1 在 Windows 上**只走 ConPTY**(Win10 1809 / build 17763+),没有 winpty/cygwin pty fallback。
- 优先用 sideloaded `conpty.dll`(从 Microsoft Store 的 Terminal 包来),否则用 `kernel32.dll` 里的版本。
- mini-term 直接用 portable-pty,这两条特性原样继承。

### 2.2 cwd 直传 CreateProcessW,**无 UNC 归一化**

源码位置: `portable-pty-0.8.1/src/cmdbuilder.rs:560-585`

```rust
pub(crate) fn current_directory(&self) -> Option<Vec<u16>> {
    let home: Option<&OsStr> = self
        .get_env("USERPROFILE")
        .filter(|path| Path::new(path).is_dir());
    let cwd: Option<&OsStr> = self.cwd.as_deref().filter(|path| Path::new(path).is_dir());
    let dir: Option<&OsStr> = cwd.or(home);

    dir.map(|dir| {
        let mut wide = vec![];
        if Path::new(dir).is_relative() {
            if let Ok(ccwd) = std::env::current_dir() {
                wide.extend(ccwd.join(dir).as_os_str().encode_wide());
            } else {
                wide.extend(dir.encode_wide());
            }
        } else {
            wide.extend(dir.encode_wide());
        }
        wide.push(0);
        wide
    })
}
```

**关键观察**:
1. **唯一的归一化逻辑**:把相对路径转换成绝对路径(用 `std::env::current_dir().join(dir)`)。
2. **对 UNC 路径无任何特殊处理**,直接 encode_wide 后传给 `CreateProcessW`。
3. **fallback 条件**:`Path::new(path).is_dir()` 过滤 — 如果 cwd 不存在 / 不是目录,会 fallback 到 `$USERPROFILE`。这意味着传 `\\wsl$\Ubuntu\home\nonexistent` 这种不存在的 WSL 路径时,portable-pty 会**静默改用 `%USERPROFILE%`**,前端难感知。

源码位置: `portable-pty-0.8.1/src/win/psuedocon.rs:133-161`(CreateProcessW 调用现场)

```rust
let cwd = cmd.current_directory();

let res = unsafe {
    CreateProcessW(
        exe.as_mut_slice().as_mut_ptr(),
        cmdline.as_mut_slice().as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        0,
        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
        cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
        cwd.as_ref()
            .map(|c| c.as_slice().as_ptr())
            .unwrap_or(ptr::null()),
        &mut si.StartupInfo,
        &mut pi,
    )
};
if res == 0 {
    let err = IoError::last_os_error();
    let msg = format!(
        "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
        cmd_os,
        cwd.as_ref().map(|c| OsString::from_wide(c)),
        err
    );
    log::error!("{}", msg);
    bail!("{}", msg);
}
```

`cwd` 直接以 wide string 形式塞入 `CreateProcessW` 的第 8 个参数(`lpCurrentDirectory`),没有任何拦截。

### 2.3 mini-term 当前调用链

`src-tauri/src/pty.rs:570-607`:
```rust
let mut cmd = CommandBuilder::new(&shell);
for arg in &args { cmd.arg(arg); }
cmd.cwd(&cwd);  // ← UNC 路径在这里直接进 portable-pty
// ... env injection
let child = pair.slave.spawn_command(cmd)?;
```

所以 mini-term 现状是:**前端传什么 cwd 给 `create_pty` 就直接传到 `CreateProcessW`**。如果前端传 `\\wsl$\Ubuntu\home\user\proj`,后端无任何转换,结果:
- 若 shell 是 `cmd.exe`:启动成功,但 cmd 打印 UNC 警告并 fallback 到 `C:\Windows`,用户看到的提示符不在 WSL 目录。
- 若 shell 是 `wsl.exe`:**理论上可以工作**,因为 wsl.exe 进程不在乎 Windows 端的 cwd(它有自己的 Linux 端 cwd),但用户没法把 Linux 路径塞进 portable-pty 的 cwd 字段。

### 2.4 portable-pty 是否已有相关 issue

通过 GitHub search API 未找到 wezterm/portable-pty 仓库里有专门的 UNC cwd issue;搜索 `portable-pty UNC` 命中的都是下游应用(如 vibeyard、tuicommander)在自己代码里做转换。portable-pty 上游似乎认为"按 CreateProcessW 文档传 UNC 就行,shell 的问题不归 pty 管"。

---

## 三、四种绕过方案对比

### 方案 A:`pushd` / 临时驱动器映射(`net use`)

**How**: 在启动 shell 前,Windows 端用 `net use <free-letter>: \\wsl$\Ubuntu` 把 UNC 挂成临时驱动器,然后用驱动器路径作为 cwd 启动 cmd.exe,shell 退出后 `net use /delete`。或者更简单,让 cmd 自己 `pushd` UNC 路径,它会临时占一个驱动器号。

**Pros**:
- 不需要新的进程模型,沿用 cmd.exe / pwsh.exe。
- 用户能用 `Z:\home\user\proj` 这种纯 Windows 路径访问 WSL 文件。

**Cons**:
- **触及全局系统状态**(已用驱动器号、登录会话级 mapping),被 Windows Terminal 团队明确拒绝(microsoft/terminal#19514 close comment by DHowett):
  > "I'm wary of us touching global system state--even for the duration of a single process--to work around a fairly narrow issue in a shell we're not intending on doing any new work on. I wouldn't ship that."
- **驱动器号会被泄露**到其他后续进程(`mapped drives are per logon session`,见 microsoft/terminal#19514 评论)。
- 找不到空闲驱动器号时失败(用户场景:已挂多个网络驱动器)。
- 进程异常退出时清理逻辑要兜底。
- **WSL 9P 性能问题不变**:从 Windows 端经驱动器映射访问 WSL 文件,仍走 9P 协议,性能远低于 WSL 内部 ext4。
- 对 `wsl.exe` 启动场景毫无帮助 — 这个方案只解 cmd 问题,WSL 内部仍要 `cd`。

### 方案 B:识别 `\\wsl$\<distro>\<unix-path>` → `wsl.exe -d <distro> --cd <unix-path>` 启动 ★ 推荐

**How**:
1. 在 `create_pty` 调用前(前端或后端)检测 cwd 是否匹配 `^\\\\(wsl\$|wsl\.localhost)\\([^\\]+)\\(.*)$`。
2. 若匹配,提取 `distro` 和 `linux-path`,重写启动命令为 `wsl.exe -d <distro> --cd <linux-path>`(可选加 `-- <linux-shell>`),cwd 字段设为 `%USERPROFILE%` 或 `C:\`(任意 Windows 端合法路径)。
3. 不匹配(普通 Windows 路径、非 WSL UNC、SSH UNC 等)走原路径。

**wsl.exe --cd 语义**(本机 `wsl --help` 输出,2026-05-25 实测,中文 Windows 11):

```
--cd <Directory>
    将指定的目录设置为当前工作目录。
    如果使用 ~ 则作用 Linux 用户的主目录。
    如果以 / 字符开始,值将被解释为绝对 Linux 路径。
    否则,该值必须是绝对 Windows 路径。
```

英文等价描述见 microsoft/WSL 官方仓库 wsl_help 资源文件(各发行版均有,Win10 1903 build 18362+ 全可用,Microsoft Store 版 WSL 在 Win10 19041+ 上一致)。

**Pros**:
- **官方支持的能力**,微软专门为这个场景设计。
- **跨 Win10/Win11 兼容**:Win10 1903+ inbox WSL 即支持,Microsoft Store 版 WSL 在所有平台都支持。
- 行为可预测:cwd 真正落在 Linux 路径,用户在 shell 内 `pwd` 看到 `/home/...`。
- **行业一致采用**(见第五节工业证据):Windows Terminal `Utils::MangleStartingDirectoryForWSL`、vibeyard#113、VS Code(隐式 — 不传 cwd 而是用 `-d <distro>` profile)。
- 不污染全局系统状态。
- 不存在 9P 中转 — wsl.exe 进程内部直接走 ext4,性能最优。

**Cons**:
- 需要在 mini-term 后端引入 WSL 路径解析逻辑(轻量,几十行 Rust)。
- 当用户在前端 shell 设置里**主动选了** `cmd.exe` / `powershell.exe` 作为 shell 时,需要决策:是强制覆盖成 `wsl.exe`,还是保留 cmd 让 UNC 警告出现?(推荐:UNC 是 WSL 路径时强制 `wsl.exe`,UI 上提示一下;不强求用户改设置。)
- 启动后用户拿到的是 Linux shell,不能再在终端里执行 Windows cmd(对 mini-term 场景这是可接受的,因为用户的预期就是"在这个 WSL 项目里开终端")。
- WSL 内 shell 选择:vibeyard 的做法是用 `wsl.exe -d <distro> -- sh -c 'echo $SHELL'` 探测一次默认 shell 并缓存。简单做法是固定用 `bash`,或让 `wsl.exe` 自己选默认。

### 方案 C:不强制转换,让用户在 shell 设置里手写 `wsl.exe -d Ubuntu --cd ~`

**How**: 文档里告诉用户:如果想在 WSL 项目里开终端,自己去 shell 设置里加一个 `wsl.exe --cd ~/proj`。前端不识别 WSL UNC。

**Pros**:
- mini-term 零代码改动。
- 用户保留完全控制权。

**Cons**:
- 用户体验差到不可用 — 每个 WSL 项目要单独配 shell,失去"添加文件夹即开终端"的核心价值。
- 用户不知道为什么默认 cmd 跑出 UNC 警告(没有诊断)。
- 跟 mini-term 多项目场景冲突(每个项目可能用不同 distro)。

### 方案 D:cwd 仍传 UNC,依赖 shell 自己处理

**How**: 不改任何代码,直接把 UNC cwd 传 portable-pty,cmd 收到警告就让它打,pwsh 能跑就跑。

**Pros**:
- 零开发成本。

**Cons**:
- cmd.exe 表现明确失败(用户看到警告并被踢回 `C:\Windows`)。
- pwsh 能启动但相对路径 / `cd ..` 行为反复无常。
- bash via Git 性能差,9P 慢得难以接受。
- 没法在终端里跑 WSL 内部的 `git`/`build`/AI 工具(违背 prd.md 里"在 WSL 项目目录里跑 WSL 内的 git/build/AI 工具"目标)。

### 总结表

| 方案 | 兼容性 | 用户体验 | 实现成本 | 副作用 | 是否符合 mini-term goal |
|---|---|---|---|---|---|
| A: pushd / net use | OK | 中(走 9P 慢) | 中 | 触及全局状态、驱动器号污染 | 否(违背"WSL 内运行"目标) |
| **B: wsl.exe --cd**(推荐) | 全(Win10 1903+) | 好 | 低 | 用户预期需对齐"WSL 路径开 = Linux shell" | **是** |
| C: 让用户手配 shell | OK | 极差 | 0 | 多项目场景不可用 | 否 |
| D: 啥都不做 | 部分 | 差(cmd 警告/pwsh 不稳) | 0 | 9P 性能问题、AI 工具找不到 | 否 |

---

## 四、`wsl.exe` 启动器细节

### 4.1 `--cd <path>` 路径语义(权威来源:本地 wsl.exe --help)

| 写法 | 解释 |
|---|---|
| `--cd /home/user/proj` | 以 `/` 起始 → 绝对 **Linux** 路径(WSL 内 ext4) |
| `--cd ~` | 解析为该 distro 当前用户的 Linux home |
| `--cd C:\Users\foo\proj` | 不以 `/` 起始 → 必须是绝对 **Windows** 路径,wsl.exe 内部转成 `/mnt/c/users/foo/proj` |
| `--cd \\wsl$\Ubuntu\home\user\proj` | **不支持**(实测:wsl.exe 会把 `\\wsl$\...` 解释为相对 Linux 路径,失败)。GH#11994 在 WT 里专门修了这个 — 见下面工业证据。 |

**优先级**:当 Linux 与 Windows 两种语义可能冲突时,以"是否以 `/` 开头"为判断标志。`./foo`、`../foo` 这种**相对路径行为不规范**,建议永远传绝对路径。

### 4.2 `--exec` vs `-- <cmd>`

| 写法 | 行为 |
|---|---|
| `wsl.exe -d Ubuntu --exec /bin/bash` | 在 distro 内非登录、非交互方式执行该命令(无 .bashrc) |
| `wsl.exe -d Ubuntu -- /bin/bash` | 把后面的当作命令行直接传 distro 默认 shell,**会经过该 distro 的 init/login 路径**(更接近"开个交互终端") |
| `wsl.exe -d Ubuntu`(无命令) | 启动 distro 默认 shell,交互式 |

**对 mini-term**:想要"开个 PTY 接入 WSL 交互 shell"应用 `wsl.exe -d <distro>` 或 `wsl.exe -d <distro> -- bash -l`。
- `-l`(login):加载 `.profile`/`.bash_profile`,设置完整环境。
- 加 `-i`(interactive)/ `-l` 与否影响 nvm/sdkman 等 PATH 补充。vibeyard PR #113 在跑 CLI 命令时显式 `bash -ic`,在普通终端时不加,这个区分值得借鉴。

### 4.3 ConPTY + wsl.exe 交互行为

- **回显 / 信号 / 大小**:wsl.exe 是 Win32 子进程,接 ConPTY 的 hPC,Linux 端用 `/dev/pts/X`,Windows 端 conhost.exe 居中桥接。**stdin/stdout/signals 都通过 ConPTY VT 序列转译**,terminfo 应设为 `xterm-256color`(mini-term 已设)。
- **Ctrl-C 信号**:ConPTY 把 Ctrl-C 翻译成 `\x03` 写入 stdin,WSL 端 line discipline 翻成 SIGINT 给前台进程组。
- **resize**:`resize_pty` 调 `ResizePseudoConsole(hpc, size)`,ConPTY 内部把 winsize 透传给 WSL 的 PTY slave,Linux 端进程收到 SIGWINCH。
- **UTF-8**:wsl.exe 默认输出 UTF-16LE,但**只在被重定向时**;接 ConPTY 时输出走 PTY 字节流,LANG=C.UTF-8 起作用。WSL_UTF8=1 环境变量在 Win10 22H2+ 强制 UTF-8(mini-term 可注入)。
- **CJK 显示**:WSL 内的 zsh prompt、git 输出走 UTF-8 字节,xterm.js + WebGL 渲染 OK。

### 4.4 跨版本兼容

| Windows / WSL 版本 | `wsl --cd` 支持 |
|---|---|
| Win10 < 1903 (build 18362) | 无 — `--cd` 选项不存在 |
| Win10 1903 inbox WSL | **支持**(但部分构建可能有 bug,Windows Terminal 在 cwd 含正斜杠时显式 mangle,见下文) |
| Win10 22H2 + Microsoft Store WSL | 完全支持 |
| Win11 全版本 | 完全支持(默认 WSL2,默认 inbox 或 Store 版) |

mini-term 的最低支持版本默认对齐 portable-pty 0.8.1 的下限(Win10 1809),但 `--cd` 需要 1903。可在前端发现 WSL 路径时探测 wsl 版本(`wsl --version` 在 1809 上不存在 → 走 fallback)。

### 4.5 几个相关 Windows Terminal 修复

- **GH#11994**: cwd 形如 `//wsl$/Ubuntu/...`(正斜杠)时,`wsl --cd` 会把它解释成相对 Linux 路径,需要主动 `make_preferred()` 换成反斜杠。WT 已修。详见第五节代码。
- **GH#12353**: `~` 不是合法 Windows 路径,只有 wsl.exe 能接受。其他 shell 拿到 `~` 作 cwd 时要 mangle 成 `%USERPROFILE%`。
- **GH#9541**: 启动前 validate startingDirectory 存在性会在 WSL 慢响应时整体卡死。WT 改成"先不 validate,启动失败再报错"。

---

## 五、业界工业级证据

### 5.1 Windows Terminal `Utils::MangleStartingDirectoryForWSL`

源码:`src/types/utils.cpp:1018-1107`(microsoft/terminal,main 分支,2026-05-25 校验)
直链:https://raw.githubusercontent.com/microsoft/terminal/main/src/types/utils.cpp

**核心逻辑**(摘自原文):

```cpp
// Function Description:
// - Promotes a starting directory provided to a WSL invocation to a commandline argument.
//   This is necessary because WSL has some modicum of support for linux-side directories (!) which
//   CreateProcess never will.
std::tuple<std::wstring, std::wstring> Utils::MangleStartingDirectoryForWSL(
    std::wstring_view commandLine,
    std::wstring_view startingDirectory)
{
    do
    {
        if (startingDirectory.size() > 0 && commandLine.size() >= 3)
        {
            const auto terminator{ commandLine.find_first_of(LR"(" )", 1) };
            const auto start{ til::at(commandLine, 0) == L'"' ? 1 : 0 };
            const std::filesystem::path executablePath{ commandLine.substr(start, terminator - start) };
            const auto executableFilename{ executablePath.filename() };
            if (executableFilename == L"wsl" || executableFilename == L"wsl.exe")
            {
                // 必须在 System32 里(防伪)
                if (executablePath.has_parent_path()) { /* check parent == system32, else break */ }

                const auto arguments{ /* rest of commandline */ };
                if (arguments.find(L"--cd") != std::wstring_view::npos)
                {
                    break; // 用户已经传了 --cd,不动
                }

                // GH#11994: //wsl$/Ubuntu/... (正斜杠)要换成反斜杠,不然
                // wsl --cd 会当作相对 Linux 路径处理
                std::wstring mangledDirectory{ startingDirectory };
                if (til::starts_with(mangledDirectory, L"//wsl$") ||
                    til::starts_with(mangledDirectory, L"//wsl.localhost"))
                {
                    mangledDirectory = std::filesystem::path{ startingDirectory }
                                       .make_preferred().wstring();
                }

                return {
                    fmt::format(LR"("{}" --cd "{}" {})",
                                executablePath.native(), mangledDirectory, arguments),
                    std::wstring{}  // ← startingDirectory 字段清空,留给 wsl --cd
                };
            }
        }
    } while (false);

    // GH#12353: ~ 不是合法 Windows 路径,只有 wsl.exe 能用
    return {
        std::wstring{ commandLine },
        startingDirectory == L"~"
            ? wil::ExpandEnvironmentStringsW<std::wstring>(L"%USERPROFILE%")
            : std::wstring{ startingDirectory }
    };
}
```

**调用现场**:`src/cascadia/TerminalConnection/ConptyConnection.cpp:163-177`:

```cpp
auto [newCommandLine, newStartingDirectory] =
    Utils::MangleStartingDirectoryForWSL(cmdline, _startingDirectory);
const auto startingDirectory = newStartingDirectory.size() > 0
                               ? newStartingDirectory.c_str()
                               : nullptr;

THROW_IF_WIN32_BOOL_FALSE(CreateProcessW(
    nullptr,
    newCommandLine.data(),
    nullptr, nullptr, false,
    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
    lpEnvironment,
    startingDirectory,    // ← WSL case 下为 nullptr,wsl --cd 接管
    &siEx.StartupInfo,
    &_piClient
));
```

**翻译为 mini-term 设计意图**:
1. 当命令行是 `wsl.exe ...` 时,把 startingDirectory 提升为 `--cd <dir>` 参数,并把 lpCurrentDirectory 设为 NULL(让父进程的 cwd 继承)。
2. 不是 wsl 时,正常传 startingDirectory 给 CreateProcessW。
3. 对正斜杠 UNC 做规范化(用 `make_preferred`)。

### 5.2 Visual Studio Code 集成终端 (Remote-WSL 之外)

源码:`src/vs/platform/terminal/node/terminalProfiles.ts`
直链:https://raw.githubusercontent.com/microsoft/vscode/main/src/vs/platform/terminal/node/terminalProfiles.ts

```typescript
async function getWslProfiles(wslPath: string, defaultProfileName: string | undefined): Promise<ITerminalProfile[]> {
  const profiles: ITerminalProfile[] = [];
  const distroOutput = await new Promise<string>((resolve, reject) => {
    cp.exec('wsl.exe -l -q', {
      encoding: 'utf16le',
      env: { ...process.env, WSL_UTF8: '0' },
      timeout: 1000
    }, (err, stdout) => {
      if (err) return reject('Problem occurred when getting wsl distros');
      resolve(stdout);
    });
  });

  const distroNames = distroOutput.split(/\r?\n/).filter(t => t.trim().length > 0);
  for (const distroName of distroNames) {
    if (distroName.startsWith('docker-desktop')) continue; // skip Docker internals
    const profileName = `${distroName} (WSL)`;
    const profile: ITerminalProfile = {
      profileName,
      path: wslPath,                              // ${system32}/wsl.exe
      args: [`-d`, `${distroName}`],              // 只传 -d,不传 --cd
      isDefault: profileName === defaultProfileName,
      icon: getWslIcon(distroName),
      isAutoDetected: false
    };
    profiles.push(profile);
  }
  return profiles;
}
```

**VS Code 模式**:为每个发现的 distro 创建独立 profile,path = `wsl.exe`,args = `["-d", "<distro>"]`,**不传 cwd**(用户在 distro home 里启动)。
- 检测前提:Win10 build 19041+(`May 2020 Update`,这是 `-d` 选项稳定的最低版本,见 `terminalProfiles.ts` line 89: `getWindowsBuildNumberAsync() >= 19041`)。
- VS Code 在 Remote-WSL 模式下走完全不同的路径(SSH-like 通道,本研究范围之外)。

### 5.3 wezterm WslDomain 模型

源码 / 文档:https://wezterm.org/config/lua/WslDomain.html

```lua
config.wsl_domains = {
  {
    name = 'WSL:Ubuntu-18.04',
    distribution = 'Ubuntu-18.04',    -- 必须匹配 wsl -l -v
    -- username = 'hunter',           -- 可选
    -- default_cwd = '/tmp',          -- 默认 Linux 路径
    -- default_prog = {'fish'},       -- 默认 shell
  },
}
```

wezterm 把每个 WSL distro 当成一个独立的 multiplexer domain。
- 自动从 `wsl -l -v` 解析生成默认 WslDomain 列表。
- 在 WslDomain 内打开新 pane 时,wezterm 知道当前 pane 的 Linux cwd,新 pane 沿用。
- 不暴露 UNC 路径给用户;路径模型直接是 distro + Linux path。

### 5.4 vibeyard PR #113 — 与 mini-term 几乎相同场景的开源参考

来源:https://github.com/elirantutia/vibeyard/pull/113(2026,Claude Code session 在 Windows WSL 项目下启动失败的修复)

错误信息(用户实际遇到):
```
CMD.EXE was started with the above path as the current directory. UNC paths are not supported.
```

**他们的实现**(已被验证可工作,1404 个测试通过):

```typescript
export function isWslPath(p: string): boolean {
  return /^[/\\]{2}WSL\$[/\\]/i.test(p);
}

interface WslPathInfo { distro: string; linuxPath: string; }

export function parseWslPath(p: string): WslPathInfo | null {
  const normalized = p.replace(/\\/g, '/');
  const m = normalized.match(/^\/\/WSL\$\/([^/]+)(\/.*)?$/i);
  if (!m) return null;
  return { distro: m[1], linuxPath: m[2] || '/' };
}

const wslShellCache = new Map<string, string>();

export function getWslDefaultShell(distro: string): string {
  if (wslShellCache.has(distro)) return wslShellCache.get(distro)!;
  try {
    const result = execFileSync('wsl.exe', ['-d', distro, '--', 'sh', '-c', 'echo $SHELL'], {
      encoding: 'utf-8',
      timeout: 2000,
      windowsHide: true,
    }).trim();
    const name = path.posix.basename(result);
    if (name) {
      wslShellCache.set(distro, name);
      return name;
    }
  } catch {}
  wslShellCache.set(distro, 'bash');
  return 'bash';
}

// spawn 时:
if (isWin && isWslPath(cwd)) {
  const wslInfo = parseWslPath(cwd);
  if (wslInfo) {
    const wslShell = getWslDefaultShell(wslInfo.distro);
    spawnShell = 'wsl.exe';
    spawnShellArgs = ['-d', wslInfo.distro, '--cd', wslInfo.linuxPath, '--', wslShell];
    spawnShellCwd = os.homedir();   // Windows 端 cwd 设为 home,避免 UNC 落到 CreateProcess
  }
}
```

**mini-term 可借鉴的要点**:
1. **正则**:`/^[/\\]{2}WSL\$[/\\]/i` 同时识别 `\\` 和 `//`,大小写不敏感。但只覆盖 `WSL$`,**`wsl.localhost` 需要单独加**。
2. **解析为 distro + linux path**:用 `replace(/\\/g, '/')` 统一分隔符,然后正则 capture。
3. **shell 探测+缓存**:`wsl -d <distro> -- sh -c 'echo $SHELL'`,2 秒超时,失败 fallback bash。整个进程生命周期内每个 distro 只探一次。
4. **cwd 处理**:Windows 端 cwd 设为 `os.homedir()` / `%USERPROFILE%`,把 Linux 端 cwd 交给 `--cd`。
5. **交互 vs 非交互**:跑 CLI 命令时 `bash -ic '<cmd>'`(加载 rc 文件);开普通终端时不加 `-ic`(因为 wsl.exe 启动默认 shell 已经是交互的)。

### 5.5 anthropics/claude-code#61460(PowerShell 下 UNC 问题)

> "code .\folder not working from powershell if working dir path is UNC"

状态 **open**,说明即使 powershell 在 UNC 下"能跑",一旦尝试启动子进程(如 `code .\folder`)就会出问题。这印证 mini-term 直接传 UNC 给 ConPTY 不可行。

### 5.6 rom1v/autoadb#33(cmd.exe 警告精确证据)

> When running autoadb from a UNC path (e.g. WSL's `\\wsl.localhost\...`), `cmd.exe` prints a warning:
> ```
> '\\wsl.localhost\Ubuntu-24.04\home\nagayasu\work\autoadb'
> CMD.EXE was started with the above path as the current directory.
> UNC paths are not supported. Defaulting to Windows directory.
> ```

修复方法:启动子进程前显式把 cwd 改成 `C:\`(autoadb 是 Rust,用 `std::process::Command::current_dir`)。这印证了**真正的限制点是 cmd.exe,而非 CreateProcessW**。

---

## 六、与 mini-term 现状的对照

### 当前 pty.rs 关键位置

`src-tauri/src/pty.rs:570-607`:
```rust
let mut cmd = CommandBuilder::new(&shell);
for arg in &args { cmd.arg(arg); }
cmd.cwd(&cwd);          // ← prd.md 标记的核心点
cmd.env("TERM", "xterm-256color");
// ...
let child = pair.slave.spawn_command(cmd)?;
```

### 需要新增的处理逻辑(给 implement agent 的草图)

伪代码,实现位置可能在 Rust 后端 `create_pty` 入口,也可能在前端 `TerminalArea` 启动前 — 推荐在 Rust 后端,因为:
- 前端可能错误显示 verbatim 前缀路径,后端拿到的更"真",检测更可靠;
- 单测在 Rust 端写更简单,所有平台 fallthrough 路径统一。

```rust
fn detect_wsl_path(path: &str) -> Option<(String /*distro*/, String /*linux_path*/)> {
    // \\wsl$\<distro>\<linux-path>  OR  \\wsl.localhost\<distro>\<linux-path>
    // 也接受 / / 分隔符和混合分隔符,以及 \\?\UNC\wsl$\... 这种 verbatim 形式
    let n = path.replace('\\', "/");
    let stripped = n.strip_prefix("//?/UNC/").unwrap_or(&n);
    let re = regex::Regex::new(r"(?i)^/{2}(wsl\$|wsl\.localhost)/([^/]+)(/.*)?$").unwrap();
    re.captures(stripped).map(|c| {
        let distro = c.get(2).unwrap().as_str().to_string();
        let linux_path = c.get(3).map(|m| m.as_str().to_string()).unwrap_or_else(|| "/".to_string());
        (distro, linux_path)
    })
}

#[cfg(windows)]
fn rewrite_for_wsl(shell: &str, args: &[String], cwd: &str)
    -> Option<(String, Vec<String>, String)>   // (new_shell, new_args, new_cwd)
{
    let (distro, linux_path) = detect_wsl_path(cwd)?;
    // wsl.exe in system32
    let wsl_exe = std::env::var("SystemRoot").map(|r| format!("{}\\System32\\wsl.exe", r))
                                             .unwrap_or_else(|_| "wsl.exe".to_string());
    // 默认 shell 用 -- bash(后续可以加 distro 默认 shell 探测+缓存)
    let mut new_args = vec![
        "-d".to_string(), distro,
        "--cd".to_string(), linux_path,
    ];
    // 如果原 shell 不是 wsl,把它当作 WSL 端 shell;否则用 bash
    // (用户在前端可能把 shell 设为 "bash"、"zsh"、或 "C:\\Windows\\System32\\wsl.exe -d ...",
    //  这里需要根据原 shell 决定如何拼)
    new_args.push("--".to_string());
    new_args.push("bash".to_string());  // 简化版,完整版应探测
    // args 是要透传给 bash 的命令(如果有的话)
    new_args.extend_from_slice(args);
    let safe_windows_cwd = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    Some((wsl_exe, new_args, safe_windows_cwd))
}
```

### 路径前缀剥离

prd.md 提到 `strip_verbatim_prefix` 只处理 `\\?\C:\` 不处理 `\\?\UNC\`。新增 WSL 检测时**也要识别 verbatim UNC**(`\\?\UNC\wsl$\Ubuntu\home\user`),否则 `canonicalize` 返回的路径漏检。

`fs.rs:113-122` 的 `strip_verbatim_prefix` 需要增强:
- 输入 `\\?\UNC\wsl$\Ubuntu\home\user` → 输出 `\\wsl$\Ubuntu\home\user`
- 输入 `\\?\UNC\server\share\folder` → 输出 `\\server\share\folder`

这样前端展示和后端 wsl 检测都拿到统一的 `\\wsl$\...` 形式。

---

## 七、Open Questions(交给主代理决策)

1. **shell 选择策略**:用户在前端 shell 设置里选了 `cmd.exe` / `powershell.exe`,而项目根是 WSL 路径,如何决策?
   - 推荐 B-1:强制改用 `wsl.exe -d <distro>`,UI 顶部小提示 "Detected WSL project, using wsl.exe to start shell"。
   - 备选 B-2:保留用户选择,但显式把 cwd 改成 `%USERPROFILE%`,然后在 shell 启动后 `cd \\wsl$\...`(对 cmd 仍触发警告,但行为可控)。
2. **WSL 内 shell 默认值**:启动 wsl.exe 后用 bash / zsh / 探测?
   - 推荐:固定 `bash`(广泛兼容),加 spec 注释"如需 zsh 用户在 shell 设置自配"。
   - 进阶:vibeyard 的 `getWslDefaultShell` 探测+缓存机制(增加一次 wsl.exe 启动开销但首次性能可接受)。
3. **路径显示**:前端展示 `\\wsl$\Ubuntu\home\user\proj` 还是 `Ubuntu:/home/user/proj`?
   - 推荐:展示 `\\wsl$\<distro>\<unix>` 形式,与 Windows Explorer 保持一致,用户复制路径能粘到资源管理器。
   - `\\?\UNC\` verbatim 前缀**必须剥**。
4. **`\\wsl$\` vs `\\wsl.localhost\`**:
   - `wsl.localhost` 是新形式(Win11 22H2+ 默认),`wsl$` 是旧形式但仍可用。
   - 推荐:检测时两种都识别,内部统一存为 `\\wsl$\` 形式(展示一致)。
5. **回归覆盖**:
   - 单测:`detect_wsl_path` 各种 corner case(verbatim、混合分隔符、wsl.localhost、空 path、非 WSL UNC、SSH UNC、本地 D 盘)
   - 集成测试:Windows 上 wsl.exe 实际可用时跑一个 WSL distro 启动的真实测试(需 CI 配 WSL)
   - Linux/macOS 回归:`detect_wsl_path` 在非 Windows 平台直接返回 None(不做误识别)

---

## 八、引用清单

### 微软官方文档

- [CreateProcessW (processthreadsapi.h)](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw) — `lpCurrentDirectory` 接受 UNC 的明确表述
- [Naming a File](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file) — `\\?\` verbatim 前缀语义、UNC 命名规则
- [WSL Basic Commands (MicrosoftDocs/WSL `WSL/basic-commands.md`)](https://raw.githubusercontent.com/MicrosoftDocs/WSL/main/WSL/basic-commands.md) — wsl.exe 全部参数
- 本机 `wsl.exe --help` 中文输出(2026-05-25 实测,Windows 11 Home China 10.0.26200):**唯一权威 `--cd` 语义说明**
- [WSL File systems documentation](https://learn.microsoft.com/en-us/windows/wsl/filesystems) — 9P 协议、`\\wsl$\` 路径介绍

### 业界开源实现

- **Windows Terminal**: `microsoft/terminal:src/types/utils.cpp:1018-1107` `Utils::MangleStartingDirectoryForWSL` ([raw link](https://raw.githubusercontent.com/microsoft/terminal/main/src/types/utils.cpp))
- **Windows Terminal**: `microsoft/terminal:src/cascadia/TerminalConnection/ConptyConnection.cpp:163-177` 调用现场 ([raw link](https://raw.githubusercontent.com/microsoft/terminal/main/src/cascadia/TerminalConnection/ConptyConnection.cpp))
- **VS Code**: `microsoft/vscode:src/vs/platform/terminal/node/terminalProfiles.ts:69-165, 280-310` `detectAvailableWindowsProfiles` + `getWslProfiles` ([raw link](https://raw.githubusercontent.com/microsoft/vscode/main/src/vs/platform/terminal/node/terminalProfiles.ts))
- **wezterm WslDomain docs**: https://wezterm.org/config/lua/WslDomain.html
- **vibeyard PR #113**: https://github.com/elirantutia/vibeyard/pull/113 — "fix: support WSL UNC paths in PTY spawn (spawnPty and spawnShellPty)"
- **autoadb #33**: https://github.com/rom1v/autoadb/pull/33 — cmd.exe UNC 警告的精确复现与修复
- **microsoft/terminal #592**: "Support linux paths for `startingDirectory` of WSL distros"(关闭,被 GH#11994 + MangleStartingDirectoryForWSL 解决)
- **microsoft/terminal #19514**: "Automatic network drive mapping when opening CMD from UNC path"(关闭,DHowett 否决 pushd/net use 方案)
- **microsoft/terminal #11994**: cwd `//wsl$/...` 正斜杠不被 wsl --cd 接受(已修)
- **microsoft/terminal #12353**: cwd `~` 在非 wsl shell 时 mangle 为 `%USERPROFILE%`(已修)
- **microsoft/terminal #9541**: WSL cwd validate 卡死问题(已修,改为不预先 validate)
- **anthropics/claude-code #61460**: PowerShell UNC cwd 启动子进程失败(open)

### portable-pty 源码(本机已下载,2026-05-25)

- `C:\Users\12197\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\portable-pty-0.8.1\src\cmdbuilder.rs:200-585` — `CommandBuilder::cwd` / `current_directory`
- `C:\Users\12197\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\portable-pty-0.8.1\src\win\psuedocon.rs:44-161` — ConPTY 加载与 `CreateProcessW` 调用
- 上游仓库:https://github.com/wez/wezterm/tree/main/pty(同代码)

### mini-term 内部代码

- `src-tauri/src/pty.rs:574` `cmd.cwd(&cwd)` — 当前直传 UNC 的核心位置
- `src-tauri/src/fs.rs:113-122` `strip_verbatim_prefix` — verbatim 前缀剥离逻辑(需扩展支持 `\\?\UNC\`)
- `src-tauri/src/fs.rs:139` `verify_under_project_root` — UNC 在 canonicalize 后会带 `\\?\UNC\` 前缀
- `src-tauri/src/fs.rs:241` `watch_directory` — `notify` 在 9P 上的潜在问题(本研究未深入,留作下个 research topic)

---

## Caveats / 未覆盖

- **wsl --cd 在 1.0.x 旧版**(微软 Store 早期版本)的 corner case 未深挖,建议在 spec 里加最低 WSL 版本约束(如 `wsl --version` 输出 ≥ 1.0)。
- **WSL 1 vs WSL 2 行为差异**:本研究只验 WSL2,WSL1 已在 prd 明确 Out of Scope。
- **跨 distro 场景**:用户机器上同时装 Ubuntu / Debian / openSUSE,在不同 distro 路径间切换时的 wslShellCache 失效条件,vibeyard 是 per-distro 缓存,mini-term 沿用即可。
- **WSL 旧路径**(`Y:` 临时驱动器映射):没研究,默认 prd 不要求。
- **Tauri dialog.open 在选 `\\wsl$\...` 目录时**:prd.md 提到 "Windows IFileDialog 理论上能选 UNC 路径",未独立验证返回的路径是 `\\wsl$\` 还是 `\\?\UNC\wsl$\`。建议 implement 阶段做一次实测,把规范化在 Rust 端做(`strip_verbatim_prefix` 扩展)。
- **portable-pty `is_dir()` 静默回退**:`current_directory()` 里如果 cwd 不存在会 fallback `$USERPROFILE`,这是 silent fallback,可能让 implement 阶段的报错难定位。若要确诊 cwd 是否真生效,后端写一段 `RUST_LOG=portable_pty=debug` 启动调试日志能看到。

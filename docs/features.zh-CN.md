<p align="center">
  <img src="../src-tauri/icons/icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>为 AI 时代打造的桌面终端管理器</strong><br>
  基于 Tauri v2 · 多项目 · 多标签 · 分屏布局 · AI 进程感知 · SSH 远程项目 · Git Worktree 管理 · 手机远程看 AI
</p>

<p align="center">
  <strong>完整功能清单 · 简体中文</strong> · <a href="features.md">English</a><br>
  <a href="../README.md">← 回到项目首页</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.12.1-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/Tauri-v2-orange" alt="tauri">
  <img src="https://img.shields.io/badge/React-19-61dafb" alt="react">
  <img src="https://img.shields.io/badge/Rust-2021-dea584" alt="rust">
</p>

---

## 解决痛点

1. **重量级工具多余** — All In AI 的用户只需要终端跑 Agent，却不得不打开 VS Code / IDEA 等重型 IDE，大且占内存
2. **多 Agent 并发无感知** — 同时开多个 Claude / Codex 会话，某个 Agent 跑完了无法直观看到
3. **项目切换不便** — 系统终端缺少多项目组织、标签页和分屏管理能力

Mini-Term 用一个轻量桌面应用解决以上所有问题。

## 预览

![主界面](screenshots/main.png)

## 功能特性

### 终端核心

- **多标签管理** — 每个项目独立标签页，拖拽排序，状态图标一目了然
- **递归分屏** — 横向 / 纵向任意嵌套分屏，Allotment 拖拽调整比例
- **高性能渲染** — xterm.js v6 + WebGL 加速，自动降级为 Canvas；启用最小对比度，修复 Claude 提问文字在暗色下与背景近乎同色不可见的问题
- **滚动缓冲行数可调** — 主缓冲区保留行数可在设置里调整（默认 1 万行，改小当场生效并释放内存；xterm 每行按列数分配内存，早期硬编码的 10 万行意味着单个终端最高吃掉两百多 MB，而终端只在关闭分屏时才销毁，多项目多分屏叠加足以把渲染进程推到 OOM），同时全局遵循标准 CSI 3J（ED3）；Codex 等应用可删除流式临时内容并重放折叠后的最终 transcript，`/clear` 也能真正清除旧历史；alternate screen 切换仍被拦截，TUI overlay 留在主缓冲区并保持滚动条可用。Windows 版内置并预载固定版本的官方 ConPTY 兼容运行时（资源校验失败时自动回退系统 ConPTY），让不同 Windows 版本下的 Codex 滚动与 transcript 折叠行为保持一致
- **终端缓存** — 切换项目 / 标签 / 分屏不重建 xterm 实例，已有内容不丢失；启动按需懒加载，仅当前可见 pane 创建 PTY，避免历史项目终端越多启动越卡
- **项目切换缓存** — FileTree / GitHistory 数据按项目缓存，切回已访问项目零延迟渲染；目录加载与 Git 状态并行执行，Git 仓库扫描结果缓存 30 秒
- **复制粘贴** — `Ctrl+Shift+C/V`（macOS `⌘+Shift+C/V`）快捷键 + 右键菜单，未选中时"复制"自动置灰；可在设置中开启「智能 `Ctrl+C/V`」（有选区时 `Ctrl+C` 复制、无选区时中断程序，`Ctrl+V` 直接粘贴）；Windows 大段多行粘贴自动分块写入，防止 ConPTY 丢行
- **拖选停留自动复制** — 拖选文本后按住鼠标静止超过设定时长（默认 1s，可调 0.2–60s，0 = 关闭）自动复制选区并在光标旁弹「已复制」气泡；松手时选区已继续增长则补复制一次，剪贴板始终是最终看到的完整选区
- **长文本粘贴** — 剪贴板文本 ≥10 行或 ≥2000 字符时自动转存为临时 `.txt` 并粘贴带引号的文件路径，避免 AI 工具直接处理超长内容引发性能与 paste bracket 问题
- **图片粘贴** — 剪贴板含截图时自动检测，通过 Win32 API 保存为临时 PNG 并粘贴带引号的路径，兼容 PinPix 等非标准格式
- **远程 / WSL 粘贴自动落地** — 上面两种「转存成文件再粘路径」的能力在远程终端里会自动换算落点：SSH 远程项目经 SFTP 把文件上传到远端目录后粘贴**远端**路径（默认 `<项目根>/.mini-term/pasted`，落在项目内 agent 无需额外授权即可读，目录可在设置中改成 `/tmp/mini-term`、`~/uploads` 等，并自动写入自忽略的 `.gitignore` 以免弄脏 `git status`）；WSL 项目则把 `C:\...` 换算为 `/mnt/c/...`（无需上传）。上传失败会明确弹提示，而不是粘一个远端读不到的本机路径
- **文件拖拽** — 文件树或系统资源管理器拖文件到终端自动插入带引号的绝对路径，精准定位目标分屏 pane，兼容含空格的路径；拖拽途中按 `Esc` 就地取消，路径不写入 PTY（这次 Esc 由 window capture 阶段吞掉，不会当成 `\x1b` 送进终端），松手也不会退化成一次普通点击把文件打开，悬停虚线框同步撤掉。只有越过 5px 阈值真正进入拖拽后才吞 Esc，别处的 Esc 照常生效
- **多 Shell 配置** — Windows（cmd / powershell / pwsh）、macOS（zsh / bash）、Linux（bash / sh）等，可自由增删

### SSH 连接

- **连接管理** — 顶栏「SSH」按钮打开管理弹窗，左侧分组列表 + 右侧连接列表的两栏结构，对 SSH 连接增删改，支持主机 / 端口 / 用户名 / 密码 / 私钥 / 分组字段，持久化到配置文件；「关联 SSH」「添加远程项目」两个弹窗与它同构（同一套分组归类逻辑，全部视图下按组折叠，全选 / 全不选只作用于当前可见连接），删除连接前弹二次确认并说明会丢失已存密码与私钥路径
- **快速连接** — 终端内右键「SSH 连接」子菜单按分组列出已保存连接，选中后在当前终端直接拼接 `ssh` 命令拉起会话
- **密码自动填充** — 配了密码的连接，后端扫描 PTY 输出命中密码提示自动回写密码，每会话只填一次，密码错误时停止以防连灌错误密码
- **私钥权限自动处理** — 使用私钥连接时自动把密钥复制到权限收紧的临时副本（Windows `icacls` / Unix `0600`），绕过 OpenSSH「UNPROTECTED PRIVATE KEY FILE」拒绝，不修改用户原始密钥文件
- **进阶能力** — 密钥文件登录（`ssh -i`）、连接分组管理：右键新增 / 重命名 / 解散分组（空分组可持久保存），拖拽连接到分组调整归属，编辑表单分组字段可下拉选择已有分组
- **SSH 工具（CLI + Skill，供 AI agent）** — 让终端里运行的 AI agent（Claude Code / Codex）能操作已保存的 SSH 连接。项目右键菜单「关联 SSH」按项目启用并限定所选连接；启用时生成 Claude / Codex 两份 SKILL.md，内嵌 CLI 绝对路径与随机项目能力令牌，自动追加 `.gitignore` 并迁移清理存量 MCP。`list` / `exec` / `upload` / `download` 每次都必须携带令牌，缺失、纯空白、未知、重复或属于已停用项目的映射一律 fail closed，绝不回退到全部连接；生成示例分别覆盖 Bash、正确转义的 WSL interop 与必须使用 `&` 调用运算符的 PowerShell。远程 stdout/stderr 与退出码原样流式透传（124 = 超时、2 = CLI 错误），SFTP 分块传输，认证凭据始终留在本机，每次调用写审计日志，并硬拒绝传输内含全部 SSH 明文凭据的 mini-term `config.json`。CLI 背后是全机单例 daemon 持久连接池（首调自动拉起、空闲 10 分钟 drain 自退、版本升级自动换代）；Ctrl+C / 客户端断开或请求超时时显式关闭对应 SSH channel，健康 session 继续留池。IPC 仅当前用户可连，安全端点无法建立时 fail closed；daemon 不可用则自动降级为进程内直连。过渡期 `mt-ssh-mcp` MCP sidecar 继续随包发布
- **SSH 远程项目** — 把远程服务器上的目录直接添加为项目管理：「添加远程项目」弹窗选择已保存的 SSH 连接并填写远程 POSIX 路径，保存前先远程验证目录存在；文件树经 SFTP 懒加载展开（展开行内 loading 反馈，支持手动刷新，根 `.gitignore` 过滤），终端 `ssh -t` 直连并自动落到项目目录，断线后覆盖层一键重连；Session 块按时间混排远程机器上的 Claude / Codex 会话并支持正文查看；引用的连接被删除时项目显示「断链」态而非静默失效；底层与 SSH 工具 sidecar 共用抽出的 `mt-ssh` crate（russh 持久会话池 + SFTP 原语），远程缓存键掺入连接 id，防止两台服务器的同名路径互相串数据

### WSL 支持（Windows）

- **WSL 目录作为项目根** — 支持把 `\\wsl$\<distro>\<unix-path>` 与 `\\wsl.localhost\<distro>\<unix-path>` 两种形式的 WSL 路径添加为项目，前端展示路径自动剥掉 `\\?\UNC\` verbatim 前缀，文件树可正常展开与预览
- **自动 wsl.exe 启动** — 检测到 cwd 是 WSL UNC 路径时，`create_pty` 忽略用户配置的 shell（cmd / pwsh 等），强制改用 `wsl.exe -d <distro> --cd <unix-path>` 启动，cwd 真正落在 WSL 里（`pwd` 显示 `/home/<user>/proj` 而不是 `C:\Windows`），与 Windows Terminal `MangleStartingDirectoryForWSL` 行为一致；distro 名从路径直接 parse，不调 `wsl -l -v` 探测；触发重写时弹一次性 toast 提示
- **已知限制** — AI 进程识别（ai-working / ai-idle 状态）依赖宿主机的 `process_monitor` 看子进程名，wsl.exe 启动后 WSL VM 内的 `claude` / `codex` 进程不在监控范围内，AI 状态会失效；`notify` 文件监听在 WSL 9P 文件系统上事件大概率丢失，文件树需要手动刷新。仅 WSL2 验证，WSL1 兼容性未保证

### 文件搜索

- **全局搜索** — `Ctrl+Shift+F`（macOS `⌘+Shift+F`）快捷键或文件树工具栏按钮唤起，支持文件名搜索和文件内容搜索两种模式
- **正则匹配** — 可切换子串 / 正则模式，结果关键词高亮显示
- **流式推送** — 后端使用 ignore crate 遍历文件树，每 50 条或 100ms 批量推送结果，支持随时取消
- **内容分组** — 内容搜索模式按文件分组展示匹配行号，点击结果直接预览并定位到匹配行

### AI 进程感知

- **Hook 事件系统** — 接入 Claude Code / Codex / Grok Build 官方 Hook API，接收 AI 工具事件（SessionStart / End、ToolUse 等），比进程轮询更精准及时；内置 `miniterm-hook` CLI 工具供 Hook 系统调用，自动 POST 事件到本地服务器；设置界面按「注入目标」勾选注册 / 卸载 Hook 配置——Claude Code / Codex / Grok 三家各一行可选，注册与卸载只作用于所选（三份配置文件互不相干，只用其中一家的用户没理由被写另外两家的配置）；每行显示该家的配置文件路径与注册现状（未注册 / 已注册 N 个事件 / 旧版本 N⁄M，黄色提示重新注册可补齐新增事件），默认勾选已经装了的那几家（老用户再点注册就是纯补齐），一家都没装过时全选保住首次一键注册的体验；写入合并而非覆盖用户已有 hook。Codex 权限请求从审批到工具执行完成期间持续保持 `ai-working`，避免提前触发任务完成提醒
- **实时状态检测** — Hook 一旦接入即为该面板的状态来源，逐轮状态直接由 hook 事件决定，不看输出活跃度（AI 空闲期 TUI 的定时重绘曾被误判为「又在工作」，导致完成通知反复触发）；无 hook 的面板降级为输入检测（识别键入的 `claude` / `codex` / `opencode` / `pi` / `grok` 命令，含 ↑ 历史与 Tab 补全的行快照兜底）加 500ms 输出活跃度轮询，显示 idle / working / error 状态
- **Grok Build 的 hook 接入** — `grok`（xAI 的终端 agent）走与 Claude / Codex 同一套 hook 链路，状态徽章、完成播报、AI 启动器与移动端发起会话全通。三处结构性差异各有对策：① grok 默认还会扫描 `~/.claude/settings.json` 的 hooks（Claude 兼容层），同一事件因此会来两趟——sidecar 按 `GROK_SESSION_ID` 加「有没有 argv」判出兼容层那趟并丢弃，而用户只注册了 Claude 时又必须放行（那是唯一来源），判据落在「原生 hook 文件是否在场」上；② 注册进 `~/.grok/hooks/` 的命令是**不含空格的裸文件名**（注册时把 hook 二进制复制进同目录），因为带空格的命令会被 grok 丢给 shell，而 Windows 上具体是 git-bash / pwsh / powershell / cmd 由环境决定、四家引号语义互斥，事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递；③ grok 没有 `PermissionRequest` 事件，「等你批准」是 `Notification` 的 `permission_prompt` 类型，归一化后点同一盏黄灯，而它的 `task_complete` 是知会不是待办，不点灯。另有一处专门抹平：grok 在会话收尾时会补发一次 `Stop`（`reason` 为 `channel_closed` / `shutdown`），不拦掉的话每次退出 grok 都要白响一声「任务完成」
- **Grok 的会话记录形态** — 与另外两家「一个文件一个会话」不同，grok 一个会话是**一整个目录**：`{grok_home}/sessions/{URL 编码的 cwd}/{session-id}/`，正文在 `updates.jsonl`（ACP 会话更新流），元信息在 `summary.json`。定位项目走**解码目录名**而不是编码项目路径（后者要逐字复刻它所用编码库的转义集；超长路径退化成 `{slug}-{hash}` 形态时回落读目录内的 `.cwd`）。正文一条消息会被拆成任意多个 chunk 行流式落盘，必须攒到边界（工具调用、回合收尾、对方开口）才算一条，否则一句回答在镜像里会碎成几十条。用量取 `turn_completed` 自带的 usage（按模型分解，ACP 口径的输入含缓存读写，拆成互斥桶后与 `totalTokens` 对齐）；**工具排行对 grok 为空**——持久化的 ACP `tool_call` 只带人类可读的 title，真正的工具名不落盘，拿 title 顶替会往排行里灌自然语言标签
- **只靠输入检测识别的 agent** — `opencode` / `pi` 没有接 hook，也没有可解析的本地会话记录：状态徽章、完成播报、AI 启动器与移动端发起会话四条链路照常可用，但对话镜像、AI 历史面板与用量统计对它们为空。镜像的启发式绑定据此设了白名单（`mobile_mirror::agent_has_session_log`），不在名单内直接返回空镜像，不会退而绑到同项目里其它 agent 最新的会话文件、把别人的对话贴到这个 pane 上。命令匹配走 basename 全等，`pip` / `ping` / `pixi` / `pi.py` 不会被误判成 `pi`
- **徽章卡死的三重兜底** — `Stop` 事件在若干情形下根本不触发：回合因 API 错误结束走 `StopFailure`（映射 ai-idle 并点黄灯提示回来重发）、用户按 Esc / Ctrl+C 打断则不发任何事件（由输入检测收敛，cause=`Interrupt`）；两者都覆盖不到的残余情况再由**停摆判定**兜底——hook 状态停在 ai-working 且状态与 PTY 输出双双静默 10 秒即收敛，此前已触发过退出（Ctrl+D / 双击 Ctrl+C / `/exit`，且之后无 hook 事件扶正）则判为已退出回落 idle，否则降为 ai-idle。三条兜底的结论都**一次性落盘**进 hook 状态，触发一次即收敛不再摆动，且 cause 一律不是 `Stop`，因此不会被当成「任务完成」播报（这正是 v0.9.3 删掉无记忆版兜底的原因）；正等用户批准的面板（如 Codex 的 `PermissionRequest`）豁免停摆判定，否则会连托盘黄灯一并抹掉
- **状态聚合** — 面板 → 标签页 → 项目逐层聚合，优先级 `error > ai-working > ai-idle > idle`
- **完成提醒三件套** — AI 任务从 working → idle、且成因确为 `Stop` 事件时立刻触发（权限请求、通知、澄清同样落到 `ai-idle`，不再被误报为任务完成；无 hook 的降级路径仍以下降沿为准）：
  - 右下角 Toast 桌面通知（仅非活跃项目弹出，同项目去重）
  - 项目列表 DONE 徽章，点击清除
  - 任务栏闪烁（Windows）/ Dock 跳动（macOS），窗口失焦时才触发
  - 提示音播放（Web Audio API 合成默认音，支持自定义音频文件）
  - 所有通知开关独立可配，在「设置 → AI → 通知提醒」页统一管理（Hook 注册另在同组的「Hook 事件」页）
- **待确认提醒** — AI 停下来等你批工具权限、填 MCP 表单，或这一轮因 API 错误结束（`PermissionRequest` / `Elicitation` / `StopFailure`，与项目行黄灯同一判定）时，走上面同一套通道再提醒一次，开关独立、默认开（「设置 → AI → 通知提醒 → 触发时机」；它的触发频率远高于完成，只想留完成通知的人得能单独关掉）。判据取黄灯的**上升沿**而非「本次成因属待确认类」：后端把这类事件显式排除在去重之外（同一轮里第二次授权请求不能被吞掉），按成因判会一次待确认响好几声；黄灯亮着期间不重复提醒，你对该终端键入即视为已在处理（黄灯清除），下一次请求才重新构成上升沿。Toast 用警告色 + 感叹号与绿色的「已完成」区分，不设 DONE 徽章（那是完成态的标）
- **托盘状态灯** — 系统托盘常驻全局 AI 状态灯：黄=待确认、蓝=处理中、绿=完成未读、灰=安静，多状态并存且窗口失焦时轮播展示；右键托盘菜单列出**所有进入 AI 会话的项目**及各自状态（含 ⚪ AI 空闲待命的，不只列有动静的；排序 待确认 > 处理中 > 已完成 > 空闲，条数上限可配，空闲只进菜单不点灯）、点某项即定位到该项目内最该处理的那个 pane，左键唤起主窗口并跳到「下一个该我处理」的会话（与标题栏状态灯同一套落点，可在设置里关掉只唤起窗口；Linux 下仅右键菜单可用）；Notification 判定只认权限 / 确认类文案，API 错误与重试等待不点黄灯；可在设置中关闭
- **会话自动续接** — 重启后每个分屏 pane 自动写入 `claude --resume` / `codex resume` / `grok --resume` 续回上次会话：会话身份由 hook 上报、随布局持久化，跨一次重启保留；写入终端前经白名单校验（仅字母数字与 `-_`、长度上限 128），远程 pane 不参与，识别不了的一律不写；可在「设置 → 系统 → 常规」关闭（关掉后终端照常恢复，只是不自动跑续接命令）
- **会话进出检测** — 命令 echo 识别进入 AI；双击 `Ctrl+C` / `Ctrl+D` 或 `exit` / `quit` / `:quit` / `/logout` 识别退出
- **会话历史** — 读取本地 Claude / Codex / Grok 历史会话记录，右键复制恢复命令快速续接；首屏仅渲染 20 条，底部「加载更多」按钮按需展开（不再滚动即触发）
- **会话查看** — 右键「查看」展示完整对话内容，User 纯文本 / Assistant Markdown 渲染（外链点击二次确认后调系统默认浏览器打开），支持 `Ctrl+F` 搜索高亮和 User 消息快速导航
- **WSL 会话** — Windows 下直接读取 WSL 发行版内的 Claude / Codex 历史会话（不 spawn `wsl.exe`，走 `\\wsl$` UNC + 注册表枚举发行版）：WSL 根项目自动推导发行版与路径零配置加载；Windows 路径项目右键「WSL 会话」子菜单选择发行版后按 `/mnt` 规则映射扫描，靠会话内 cwd 精确校验防串项目；WSL 会话与本机会话按时间混排并带 WSL 标识，加载中头部显示 spinner，查看正文同样支持
- **AI 任务标记** — AI 会话内每次用户按 Enter 自动在 xterm 打点，标签右上角 ⚑ 按钮下拉展示历史提交列表，点击或 `Ctrl+Shift+↑/↓`（macOS `⌘+Shift+↑/↓`）在标记间跳转，目标行短暂高亮提示

### 使用统计

- **多维聚合面板** — 顶栏「统计」打开：Claude Code / Codex / Grok 的成本、调用次数、会话数三组 KPI，按日 / 按小时趋势图（recharts），模型排行、项目排行与 Top 会话；agent / 时间范围 / 项目过滤随手切换
- **rusqlite 本地账本** — 本地会话 JSONL 解析进 SQLite 账本，面板查询毫秒级返回，打开与常驻期间后台增量同步（文件指纹变化才重解析）；账本定位为「可从原始记录再生的缓存」，损坏自动重建，无迁移负担
- **计费准确性** — fork 复制的历史消息按血缘去重，不重复计费；缓存写 / 缓存读按官方价差精确计价（1h 缓存写 2× 输入价、1h 子集只补差价）；未知模型按 Claude 主力档均价估算
- **价格表** — 每日从 models.dev 拉取一次公开价目（只读 GET，**不上传任何用量数据**），拉取失败回退本地缓存，面板绝不显示凭空编造的数字

### 移动端 + 自托管中转

出门在外用手机看桌面上跑着的 AI，并直接给它发指令。

**前提**：需要一台你自己的、可公网访问的服务器来跑中转（1C1G 足够，Docker 一条命令起，另需一个解析到它的域名做 TLS，见[部署文档](deploy-relay.zh-CN.md)）。

- **一站式连接与配对** — 顶栏「移动端」面板里填中转地址 → 保存并连接 → 生成配对二维码，全流程一个面板走完；手机相机扫码即打开 PWA 自动配对，配对码一次性有效（10 分钟），新设备配对自动顶替旧设备，「重置配对」立即吊销全部凭证
- **活跃 AI 会话列表** — 手机端按项目分组展示正在跑的 Claude / Codex / Grok 会话，状态灯与桌面端实时同步增删变色；桌面端离线时顶部横幅提示并置灰，恢复后自动消除
- **手机发起新会话** — 右下角 + → 选项目 → 选 AI 启动器，桌面端在该项目后台开一个终端标签并把 agent 拉起来，会话真起来后手机自动进入它的对话镜像（不打断你桌面上正在看的现场）；项目按桌面端的分组层级展示，可折叠。启动器是桌面端配置的具名条目，手机只按 id 引用、看得到名字，**命令文本从不经过手机或中转**
- **会话重命名** — 手机上给会话改个看得懂的名字（列表行的 ✎ 或镜像页标题），同步显示在桌面端的终端标签上；留空恢复默认名
- **对话镜像（只读）** — 点进任一会话实时查看对话内容，AI 回复 Markdown 渲染、桌面输入原文展示，滚动到顶自动分页加载更早消息；镜像绑定经 Hook 会话身份精确到 pane，同项目并行开多个 AI 也不会互相串台
- **移动端指令** — 镜像页底部输入框把文本写穿到桌面对应终端（等价于本人在键盘上敲下并回车），带即时回执与明确失败原因；桌面端离线时中转直接拒绝，不做存储转发
- **中转只转发不落盘** — 中转服务器不存储任何消息体，日志仅记录元数据（有子进程级自动化测试断言全流程零文件残留）；自带三阶段 Dockerfile 与 compose 示例，一条命令从源码构建启动，反代 + TLS 配置见 [部署文档](deploy-relay.zh-CN.md)
- **PWA 体验** — 手机浏览器「添加到主屏幕」后以独立窗口运行，断线指数退避重连并自动恢复订阅，内置与桌面端同模式的中英双语

### 项目管理

- **项目列表** — 左侧边栏管理多个项目目录，一键切换工作区，重启自动恢复上次激活项目
- **项目描述** — 右键「编辑描述」给项目补一行说明，项目名后灰色小字展示；一排 worktree 子项目各自在干什么一眼分清
- **项目行图标** — 项目行显示技术栈图标与该项目正在跑的 AI 品牌图标（按厂商去重、字母序排列，单色品牌图标上品牌色），pane 标签与会话列表同步展示品牌图标
- **悬停 pane 预览** — **仅限跑着 AI 会话的项目**（判定与项目行 AI 品牌图标同口径 `paneShowsAiSession`：行上亮着图标才有预览；AI 退出后浮层随即收起，普通 shell 项目悬停只出绝对路径 tooltip，不弹卡打断视线）。悬停项目行 250ms 弹出该项目终端区的**微缩布局拼图**：按 SplitNode 树的 flex-grow 复现真实分屏比例，浮层固定宽度永不超屏，与切过去看到的所见即所得；打开期间 500ms 重画，预览是活的。走「读 buffer 自绘」而非截图 —— 隐藏 pane 的 xterm 实例渲染是暂停的、WebGL 也没有 `preserveDrawingBuffer`，像素不可靠，但它的 buffer 一直被全局 `pty-output` 监听实时更新，于是按结构性接口提取 viewport 的同色 run（宽字符单独成 run、粗体标准色亮化、256 色 / truecolor 解析）后按 8px cell 网格绘制位图再等比缩放。每个分屏叶子显示当前 tab 的画面（cover + 左下锚定，保住最新输出与 TUI 输入区），隐藏 tab 以「+N」徽章示数并附其中最高优先级的状态点（error > ai-working > ai-idle，与 store 聚合同口径）——藏在非激活 tab 里的 AI 状态不漏报；未起 PTY 的 pane 显示「未启动」占位（项目绝对路径在卡头可见，没浮层时退回行 title）。**非激活的 pane tab** 悬停 250ms 同样弹单格缩略图浮层（`PaneTabPreview.tsx`，同一渲染链路：缓存终端读 buffer → run 提取 → canvas 位图，打开期间 500ms 重画；未启动占位与远程断线遮罩同 MiniPane 口径），且**不做 AI 开闸**——隐藏 tab 的内容不切过去本来就看不见，预览回答的就是「那个 tab 里现在是什么」。触发时序与项目行预览同一套（250ms 定时器、到点取 rect、`isConnected` 判活），移出/点击/右键/滚动即关，收起走「渲染 gate + effect 收 state」双闸（tab 被 X 关掉时点击被 stopPropagation 拦下，只留渲染 gate 会残留旧锚点）；卡片钳制左右边界，底部分屏放不下时翻到 tab 上方
- **拖拽添加项目** — 从资源管理器拖拽文件夹到项目列表即可快速添加，自动识别文件 / 文件夹 / 重复项目并给出视觉反馈
- **嵌套分组** — 最多 3 级项目分组，拖拽排序，折叠 / 展开，分组右键菜单可直接添加本地项目或远程 SSH 项目并归入该组（折叠的分组自动展开）；「删除分组」先弹确认并说明组内项目会移到上一级而非被删除；「移动到分组」按分组树逐级展开子菜单，当前所在组标 ✓ 并置灰，超深度的组不可选
- **Worktree 子项目** — worktree「设为项目」后挂在主项目下方作子项目（缩进跟随分组），拖出或右键「脱离父项目」可转回顶层，删除父项目时子项目原位晋升不丢失；项目列表为 worktree 项目显示 ⎇ 分支徽章，仓库列表与 Changes 下拉同样标注 worktree 条目；**外部删除的 worktree 自动收敛** —— 窗口重获焦点时探测子项目目录是否还在，AI agent 在终端里跑完 `git worktree remove` 后，目录已消失的子项目连同终端资源一并移除，⎇ 徽章同步重探（仅在父项目目录仍存在时清理，盘符掉线不会误删；SSH 远程与 UNC/WSL 路径不参与），worktree 弹窗「清理失效条目」也会一并移除指向它的项目
- **文件树** — 集成目录浏览器，自然排序（V1 → V2 → V10 而非字典序），嵌套 `.gitignore` 置灰（每层子目录的忽略规则与 `!pattern` 白名单都会生效，与 git 行为一致），`notify` 文件监听实时刷新
- **文件操作** — 文件树内新建文件 / 文件夹、重命名、删除、查看内容（Markdown 渲染支持 HTML 标签和外部图片，外链点击二次确认后调系统默认浏览器打开，图片格式直接展示，HTML 文件 iframe 预览并自动解析相对路径资源，二进制与超大文件友好提示）
- **内置文件编辑器** — 文件树点开文件即可就地编辑（CodeMirror 6 内核）：140+ 语言语法高亮按文件类型自动匹配、按需懒加载，查找替换（`Ctrl+F`，面板中文化）、代码折叠、括号匹配、多光标；`Ctrl+S` 原子落盘（临时文件 + rename，不怕写坏），CRLF 文件按原行尾往返不产生全文件 diff；有未保存修改时关闭 / 跳转先确认，文件被外部改动时干净则静默重载、脏则出提示条；Markdown / HTML 预览实时渲染未保存草稿；语法配色经 `--syn-*` 变量转引应用色板，自动跟随四套主题皮肤
- **外部编辑器打开** — 文件树右上角按钮一键用配置的编辑器（默认 VS Code）打开当前项目，路径可在「设置 → 系统 → 外部编辑器」自定义；文件可用系统默认应用打开
- **项目级环境变量** — 项目右键菜单「环境变量…」打开管理弹窗，行级 `[启用 checkbox][key][value][✕]` 布局，启动该项目终端时按项目注入到 PTY 子进程；严格 POSIX 校验（key 匹配 `^[A-Za-z_][A-Za-z0-9_]*$`、非 `MINITERM_` 前缀、不可用 `WSLENV`、项目内不重复，value 禁 `\n/\r/\0`）；Rust 端再加 `MINITERM_` 前缀 + `WSLENV` 防御性过滤，即便手改 `config.json` 绕过前端校验也无法破坏 hook 协议或 WSLENV 拼接；WSL 项目下环境变量通过 WSLENV 机制透传至 Linux bash（`/u` 单向不做路径翻译；`~/.bashrc` 中 `export` 同名变量会覆盖）

### Git 集成

- **文件状态** — 文件树显示 Git 状态颜色（修改 / 新增 / 删除 / 冲突）
- **变更 / 历史同屏** — Git 面板为上下两个可折叠区块：更改在上、提交历史在下，中缝可拖拽调节比例（钳 15%~85%），折叠 / 展开带动画且会话内记住折叠态与比例；面板顶部仓库栏下拉切换仓库（worktree 条目标 ⎇），分支徽章点击只切历史查看分支（不 checkout，查看非 HEAD 分支时高亮提示），刷新 / Pull / Push 集中在栏上，右键仓库名可在终端打开或进入 Worktree 管理
- **变更 Diff** — 工作区文件变更的详细 Diff，Hunk 行级解析，并排 / 内联双视图，并排模式支持拖拽调节分隔比例，字号跟随终端字体设置
- **提交历史** — 平铺展示顶部仓库栏选中仓库的提交记录，游标分页加载（默认 30 条）
- **分支拓扑图** — 提交历史每行左侧绘制 SVG 拓扑图，按 lane 布局画出分支、合并与直穿连线，节点按 lane 上色、合并提交实心点套外环，汇入线用分支自身颜色的贝塞尔曲线并在根部渐变融入主线；后端 revwalk 追加 TOPOLOGICAL 排序，避免时钟偏移或 rebase 后父提交排在子提交之前导致连线断裂；commit 行只标注本仓库自己检出的分支，不再把其他工作区 / 远程分支全挂上来
- **提交 Diff** — 查看任意提交的文件变更，逐文件切换
- **分支信息** — 本地 / 远程分支列表
- **源码控制面板** — VS Code 风格 Changes 面板，Staged / Changes / Untracked 分组展示，支持单文件和全量 stage / unstage / discard，`Ctrl+Enter` 快速提交，列表与树形视图切换
- **Pull / Push** — 顶部仓库栏按钮一键同步远端，刷新按钮重新加载提交记录与分支信息
- **多仓库发现** — 自动扫描项目目录下所有 Git 仓库（递归 5 层，跳过 `node_modules` 等）
- **Worktree 管理** — 项目右键菜单或 Git 面板顶部仓库栏右键打开「Worktree 管理」弹窗：列出全部 worktree、基于现有分支或新建分支创建、删除（可强制）、清理失效条目，增删后即时刷新仓库列表；worktree 可一键「设为项目」或直接在终端打开，pane 支持工作目录覆盖并随布局持久化、分屏继承目录。项目根目录本身不是仓库时会向下扫描子仓库，按主工作区归并为分组列表，组头可勾选多选 / 全选，一次为每个勾选的仓库各建一个 worktree（分支下拉取各仓库分支交集，路径框语义变为父目录并预览 `<仓库名>-<分支>` 落点，失败的逐仓库列出错误）

![Git 集成](screenshots/git.png)

### 外观与配置

- **图标侧栏 + 三栏布局** — 最左侧常驻图标栏（折叠中间栏 / Sessions / Git / 设置 / SSH）；中间栏纵向叠放 Projects 与 Files、可整栏一键折叠；右侧为终端。Sessions / Git 改为从右边缘滑出、浮在终端之上的悬浮抽屉（互斥单开，左缘可拖拽调宽并持久化，✕ 关闭），激活态蓝色竖条指示
- **三种主题模式** — Auto（跟随系统）/ Light / Dark，深色基于 Warm Carbon 暖炭色调，自定义 CSS 变量体系；标题栏由应用自绘、配色直接吃主题变量，启动深色用户无首帧浅色闪烁（Windows 窗口边框仍走 DWM Immersive Dark Mode 同步）
- **自定义标题栏** — 窗口去掉系统装饰（`decorations: false`）改由应用自绘 32px 顶栏，左侧应用名、版本号、项目切换器与全局状态灯，右侧窗口控制，配色跟随主题而不再是系统那条灰白。按平台适配窗口习惯：
  - **Windows / Linux** — 最小化 / 最大化 / 关闭三键靠右，关闭键悬停变红。Win11 的**贴靠布局**照常可用：`window_snap.rs` 子类化窗口过程，在 `WM_NCHITTEST` 中对最大化按钮矩形返回 `HTMAXBUTTON`，悬停即弹分屏菜单；该矩形随之成为非客户区、收不到 WebView 事件，悬停高亮改由后端 `titlebar-max-hover` 事件回传前端，点击直接投 `WM_SYSCOMMAND`
  - **macOS** — 保留系统原生交通灯（`titleBarStyle: Overlay` + `hiddenTitle`），左上角留出让位，不自绘三色圆点，全屏 / 手势 / 系统集成一并保住
  - **项目切换器** — 版本号右侧以竖线隔开的胶囊按钮，常显当前项目名与它自己的 AI 状态色点（没有 AI 会话时压暗）；下拉列出所有进入 AI 会话的项目及状态（与托盘右键菜单同一份聚合，按 待确认 > 处理中 > 已完成 > 空闲 排序），点击项目即切换并定位到该项目内最该处理的 pane，全都安静时只切换项目
  - **全局状态灯** — 紧挨项目切换器右侧，汇总所有项目所有 pane 的最紧急一档（异常 > 待确认 > 处理中 > 已完成），点击跳到「下一个该我处理」的会话：待确认 / 异常优先，其次是**最先完成**的那个，最后才是还在跑的。与托盘右键菜单的排序有意不同——托盘回答「哪些项目还活着」，状态灯回答「下一件该做什么」
  - 拖拽走 Tauri `startDragging` 而非 `-webkit-app-region`，避开 WebView2 模态循环导致的输入锁定（v0.2.16 修过的老问题）；双击顶栏最大化 / 还原
- **Blueprint 蓝图皮肤** — 可选科幻风蓝图皮肤，网格背景 + 角标记 + 光晕效果，支持深色 / 日间两种模式，终端配色同步切换
- **外置主题包（Dream Skin 兼容）** — 「设置 → 外观 → 主题与语言」可从文件夹或 zip 导入第三方皮肤，落在 `{app_data_dir}/themes/<themeId>/`（`theme.json` 必需，`theme.css` / 背景图可选）。同一区的「生成示例」把一份可直接改的示例皮肤写进 `themes/example/`（`theme.json` + `theme.css` + 逐字段说明的 `README.md`，改完保存即热重载）；示例内容与仓库 [`docs/theme-pack-example/`](theme-pack-example/) 是**同一份文件**（`include_str!` 编译期嵌入，文档与产物不会漂开），目录已存在时报错而非覆盖，用户改过的那份不会被静默抹掉。包内带 `manifest.json` 时逐文件核对 bytes + sha256 防损坏；导入先落暂存目录、校验通过才原子换入，坏包不会连累同名的既有皮肤。皮肤的明暗由作者在 `theme.json` 的 `appearance` 定死，激活期间内置主题按钮置为未选中态。改动包内文件即热重载（300ms 防抖）。皮肤可声明背景图，此时终端底色转半透明压在氛围层上，WebGL 渲染退回 DOM（上游 canvas 不透明限制），切回不透明主题自动恢复；DOM 路径的字符格宽按整设备像素量化与 WebGL 对齐、终端字体栈显式补 CJK 回退字体，字距与全角标点均与内置主题一致，不再显得更松；此时作者写在 `terminal.background` 的底色会被忽略——它在展开顺序上排在透明化之后，照着内置主题抄全 24 字段的皮肤本会把氛围图整块盖死。导入的 `theme.css` 过卫生检查：256KB 上限、禁 `@import`、指向包外的引用一律拒 —— 检查在剥掉注释、还原 CSS 转义后的取样上做，`url()` 与裸字符串双查（Chromium 认 `image-set("https://…" 1x)`，不带 `url()` 照样发请求），`url(\68 ttps://…)` 这类转义写法同样挡得住。`theme.json` 的 `tokens` 逃生舱走同一把尺子：键名限 `--` 开头的 CSS 变量、值过同一道闸——键名不带 `--` 时 `setProperty` 设的是**真实 CSS 属性**，否则一行 `{"background-image":"url(https://…)"}` 就绕开了上面所有检查（`tauri.conf.json` 的 csp 为 null，这是唯一一道闸）
- **字体独立调节** — UI 与终端的字号（10-20px）/ 字体 family 分别可调，终端可选是否跟随 UI 主题
- **连体字 (ligatures)** — 终端连体字渲染开关，开启后 `==` `=>` `!=` `->` 等合成 ligature glyph，需字体含 calt 表（Fira Code / JetBrains Mono）；Windows 完整支持，macOS / Linux 受 webview API 限制使用 60 条 Iosevka fallback
- **布局持久化** — 分屏比例、标签页、窗口大小 / 位置自动保存，重启恢复（`tauri-plugin-window-state`）
- **关闭确认** — 关闭窗口时只按 AI 会话数量盘点（ai-working / ai-idle 的 pane），裸 shell 终端不计入，仅当存在 AI 会话时才弹确认并列出会话名清单；无论是否弹窗都会 flush 所有项目布局
- **版本检查** — 启动时拉取 GitHub Release，有新版本时侧栏图标高亮提示、点击前往下载；版本号写入原生窗口标题
- **中英双语界面** — 「设置 → 外观 → 主题与语言」一键切换中 / 英文，整个界面实时重渲染；首次启动按系统语言自动探测并记忆选择，重启保留。每个页面、每个功能的文案均已翻译，内置轻量 i18n 层（无额外运行时依赖）
- **设置中心** — 统一的 SettingsModal，侧栏为「分组 + 分页」两级菜单：终端（Shell / 复制粘贴）、外观（主题与语言 / 字体）、AI（通知提醒 / Hook 事件）、系统（常规 / 外部编辑器），快捷键与关于留在顶级。按主题归组后每页只剩一屏左右，不再出现「一页塞九组控件、找个开关要滚半页」的老问题；分页 id 保留旧值，外部深链（`initialPage`）不因重排失效
- **满屏图标体系** — 文件树 Material 主题文件 / 文件夹图标（含目录展开态），全量图标数据（gzip ≈1.2MB）独立 chunk 动态 import 按需加载，主包零增量，未就绪时回退原手绘符号；AI 品牌图标按需深路径引入纯 SVG 组件
- **启动性能** — 字体本地打包（@fontsource woff2 随安装包分发，移除 Google Fonts 渲染阻塞外链），启动路径零网络请求、离线首帧不再挂等字体；五个重型弹窗（设置 / 文件查看 / 会话查看 / 移动端 / 统计）React.lazy 按需加载，主包 gzip 631KB → 378KB；Rust / WebView 统一时间轴启动埋点写 stderr，便于回归定位
- **界面动效** — 弹窗 / 右键菜单 / 侧拉抽屉共用一套进出场动画：遮罩淡入、面板落下并放大到位，关闭时反向播完再卸载（期间冻结内容、退出覆盖物栈，不会在淡出中变空或仍吃 Esc）；右键菜单从光标位置展开，切换终端与新建分屏各有过渡。系统关掉窗口动画（`prefers-reduced-motion: reduce`）时这套转场照常保留，用量统计面板的数字滚动与图表补间同样豁免，只停掉状态点闪烁一类的循环动画

## 技术栈

| 层 | 技术 |
|---|---|
| 框架 | Tauri v2（Rust 后端 + WebView 前端） |
| 前端 | React 19 + TypeScript 5.8 + Tailwind CSS v4 + Vite 7 |
| 终端 | xterm.js v6（WebGL addon，Canvas 降级） |
| 状态 | Zustand（全局单一 Store） |
| 布局 | Allotment（三栏主布局 + 递归 SplitNode 分屏树） |
| PTY | portable-pty 0.8 |
| Git | git2 0.19 |
| 文件监听 | notify 7 + ignore 0.4（.gitignore 过滤） |
| 用量统计 | rusqlite 0.40 本地账本 · recharts 3 趋势图 · chrono-tz 时区分桶 |
| Tauri 插件 | `window-state` · `clipboard-manager` · `dialog` · `opener` |
| 移动端中转 | axum + tokio WebSocket 中转服务（`relay-server/`）· React + TS + Vite PWA（`mobile/`） |
| 测试覆盖 | 618 个 Rust 测试 = 桌面端 565（tauri-app 411 + mt-core 44 + mt-ssh 26 + mt-sidecars 84）+ 中转服务端 53（协议与路由）；另有 100 个 Node 测试 |

## 快速开始

### 直接下载

前往 [Releases](https://github.com/dreamlonglll/mini-term/releases) 页面下载最新安装包。

> **平台支持说明**
> - **Windows** — 主要支持平台，保证可用性，日常开发与测试均在 Windows 上进行
> - **macOS / Linux** — 代码层面已支持（Tauri bundle targets = `all`），但**可用性欠佳**，未经充分打磨，欢迎提 Issue 反馈

#### macOS 安装提示

下载 `.dmg` 后双击打开,如果系统弹出 **"Mini-Term" is damaged and can't be opened. You should move it to the Bin**(已损坏,移到废纸篓),这并不是文件真的损坏 —— 而是 Release 产物没有 Apple Developer ID 签名,被 Gatekeeper 因 quarantine 标记拒绝。

把 `.app` 拖入 `/Applications` 后,在终端执行一次即可解除限制:

```bash
xattr -cr /Applications/Mini-Term.app
```

之后正常双击启动。每次升级新版本都需要再执行一次。

### 从源码构建

#### 前置条件

- [Node.js](https://nodejs.org/) >= 20.19（或 >= 22.12）—— Vite 7 的 engines 要求，CI 使用 Node 22
- [Rust](https://www.rust-lang.org/tools/install) >= 1.95 —— 由 libsqlite3-sys 0.38 使用的 `cfg_select!` 决定（1.95 起稳定；Tauri v2 自身只要求 1.77.2）
- [Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/)

#### 安装与运行

```bash
# 克隆仓库
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term

# 安装依赖
npm install

# 启动完整 Tauri 开发环境（前端 + 后端）
npm run tauri dev

# 构建发布包
npm run tauri build
```

## 项目结构

```
mini-term/
├── src/                          # 前端源码
│   ├── App.tsx                   # 主布局入口（ActivityBar + 两栏 Allotment + 悬浮抽屉）+ 窗口事件
│   ├── store.ts                  # Zustand 全局状态 + 持久化
│   ├── types.ts                  # 类型定义（Pane / Tab / Project / SplitNode ...）
│   ├── styles.css                # 全局样式 + CSS 变量（Warm Carbon）
│   ├── components/
│   │   ├── ActivityBar.tsx       # 最左侧常驻图标栏（面板开关 + AI 状态角标）
│   │   ├── RightDrawer.tsx       # 右边缘滑出的悬浮抽屉（Sessions / Git 互斥单开）
│   │   ├── ProjectList.tsx       # 项目列表 + 嵌套分组 + DONE 徽章
│   │   ├── AddRemoteProjectModal.tsx # 添加 SSH 远程项目弹窗（选连接 + 远程路径验证）
│   │   ├── ProjectEnvVarsModal.tsx   # 项目级环境变量管理弹窗（POSIX 校验）
│   │   ├── SessionList.tsx       # AI 会话历史列表（Claude / Codex / Grok）
│   │   ├── FileTree.tsx          # 文件目录树 + Git 状态 + 新建 / 重命名
│   │   ├── TerminalArea.tsx      # 标签管理 + 分屏树操作
│   │   ├── SplitLayout.tsx       # 递归渲染 SplitNode 分屏树
│   │   ├── TerminalInstance.tsx  # xterm.js 实例 + 右键菜单 + 文件拖拽
│   │   ├── PaneGroup.tsx         # 分屏分组容器
│   │   ├── MarkerList.tsx        # AI 任务标记下拉列表
│   │   ├── GitHistory.tsx        # Git 面板容器：仓库栏 + 更改/历史折叠区块 + Pull / Push
│   │   ├── GitHistoryContent.tsx # 选中仓库的提交历史列表渲染
│   │   ├── GitChanges.tsx        # 源码控制面板（stage / unstage / commit）
│   │   ├── CommitDiffModal.tsx   # 提交 Diff 查看器
│   │   ├── DiffModal.tsx         # 工作区文件 Diff 查看器
│   │   ├── SearchModal.tsx       # 全局文件搜索弹窗
│   │   ├── FileViewerModal.tsx   # 文件内容查看器
│   │   ├── SessionViewerModal.tsx # AI 会话内容查看器（Markdown 渲染）
│   │   ├── SshModal.tsx          # SSH 连接管理弹窗（分组 + 连接增删改）
│   │   ├── SshAssocModal.tsx     # 项目关联 SSH（按项目启用 MCP + 限定可见范围）
│   │   ├── MobileRelayModal.tsx  # 「移动端」面板（中转地址 / 连接状态 / 配对二维码 / AI 启动器）
│   │   ├── AiLauncherSection.tsx # AI 启动器增删改（名称 / shell / 命令 + 命令识别警告）
│   │   ├── RelayStatusBadge.tsx  # 中转连接状态角标
│   │   ├── SettingsModal.tsx     # 设置弹窗（两级菜单：终端 / 外观 / AI / 系统 + 快捷键 / 关于）
│   │   ├── LanguageToggle.tsx    # 中英语言切换
│   │   ├── ToastContainer.tsx    # AI 完成 / 待确认 Toast 通知
│   │   ├── DoneTag.tsx           # 项目列表 DONE 徽章
│   │   └── StatusDot.tsx         # 状态指示点
│   ├── hooks/
│   │   ├── useTauriEvent.ts      # Tauri 事件订阅封装
│   │   ├── useAiSubmitMarker.ts  # AI 会话 Enter 打点
│   │   ├── useExternalFileDrop.ts # 系统资源管理器拖拽文件到终端
│   │   └── useMarkerHotkeys.ts   # 标记间跳转快捷键
│   ├── i18n/                     # 自研轻量 i18n（locales/<ns>.ts 字典 + useT()）
│   └── utils/                    # 以下为节选，完整 24 个见目录
│       ├── contextMenu.ts        # 右键菜单 DOM 实现
│       ├── terminalCache.ts      # xterm 缓存 + 复制粘贴 + 长文本 / 图片粘贴
│       ├── pastePath.ts          # 粘贴落点解析（本地 / WSL 换算 / SSH 远程上传）
│       ├── wslPath.ts            # WSL UNC 判别 + Windows 路径转 /mnt 形式
│       ├── terminalSnapshot.ts   # 终端内容快照（布局恢复用）
│       ├── projectTree.ts        # 项目树递归操作
│       ├── projectDataCache.ts   # FileTree / GitHistory 项目级数据缓存
│       ├── projectEnv.ts         # 项目级环境变量校验
│       ├── remoteProject.ts      # SSH 远程项目辅助（判别 / 断链检测 / 远程 PTY 创建）
│       ├── wslPath.ts            # WSL UNC 路径解析与展示
│       ├── mobileSessionSync.ts  # 项目与活跃 AI 会话快照同步给中转（含分组层级）
│       ├── mobileStartSession.ts  # 移动端发起会话的桌面端落地（建 pane + 写启动命令）
│       ├── ptyWriteQueue.ts      # PTY 写入队列（大段粘贴分块）
│       ├── themeManager.ts       # 主题切换 + 系统配色监听
│       ├── builtinThemes.ts      # 6 套内置终端配色的统一出口（主题描述层）
│       ├── themePackManager.ts   # 外置皮肤：校验 / token 映射 / 背景层 / theme.css 外链闸 / 热重载
│       ├── panePreview.ts        # pane 预览：xterm buffer → 同色 run 网格（纯逻辑，可直测）
│       ├── panePreviewCanvas.ts  # pane 预览：run 网格 → canvas 位图（8px cell 后等比缩放）
│       └── updateChecker.ts      # GitHub Release 版本检查
├── src-tauri/                    # Rust 后端（Tauri 应用 + 共享 crate + sidecar）
│   ├── src/
│   │   ├── lib.rs                # Tauri 初始化与命令 / 插件注册
│   │   ├── pty.rs                # PTY 生命周期 + AI 会话识别
│   │   ├── conpty_bootstrap.rs   # Windows 内置 ConPTY 运行时预载（校验失败回退系统 ConPTY）
│   │   ├── process_monitor.rs    # 子进程状态轮询（500ms）+ Hook 优先
│   │   ├── config.rs             # 配置持久化 + 版本迁移
│   │   ├── fs.rs                 # 目录列表 / 监听 / 新建 / 重命名 / 删除
│   │   ├── git.rs                # Git 操作（状态 / Diff / Log / Pull / Push）
│   │   ├── search.rs             # 全局文件搜索（文件名 + 内容，流式推送）
│   │   ├── clipboard.rs          # 剪贴板图片读取 + 长文本转存临时文件
│   │   ├── editor.rs             # 外部编辑器 / 系统默认应用打开
│   │   ├── ai_sessions.rs        # Claude / Codex / Grok 会话记录读取（本机 + WSL UNC，Grok 仅本机）
│   │   ├── wsl_distros.rs        # WSL 发行版枚举（读注册表 Lxss，不 spawn wsl.exe）
│   │   ├── theme_packs.rs        # 外置皮肤目录扫描 / 文件夹与 zip 导入 / manifest sha256 校验
│   │   ├── hook_server.rs        # Hook HTTP 服务器（接收 AI 工具事件）
│   │   ├── hook_registry.rs      # Hook 注册 / 卸载（Claude Code + Codex + Grok，按 CLI 勾选）
│   │   ├── ssh.rs                # SSH 连接管理 + 密码自动填充 / 私钥处理
│   │   ├── remote_ssh.rs         # SSH 远程项目（SFTP 列目录 / 目录验证 / 远程会话读取）
│   │   ├── ssh_skill_registry.rs # 按项目启用 SSH 工具（生成 Claude / Codex 两份 SKILL.md）
│   │   ├── ssh_mcp_registry.rs   # 历史 MCP 注册的读侧清理（存量项目迁移兜底）
│   │   ├── mobile_relay.rs       # 移动端中转（出站 WSS 长连 / 配对 / 会话快照 / 指令写穿 / 发起会话 / 改名）
│   │   ├── mobile_mirror.rs      # 对话镜像（会话 JSONL 增量解析 + 分页取数）
│   │   ├── window_theme.rs       # Windows 窗口边框深色模式（DWM Immersive Dark Mode）
│   │   ├── window_snap.rs        # Win11 贴靠布局（无边框窗口下的 HTMAXBUTTON 命中测试）
│   │   └── window_input_recovery.rs # 窗口输入焦点异常恢复
│   ├── mt-core/                  # 无 tauri 依赖的共享库 crate（SSH 类型 / 配置 / 私钥）
│   ├── mt-ssh/                   # SSH 共享 crate（russh 持久会话池 + SFTP 原语，主程序与 sidecar 共用）
│   └── mt-sidecars/src/bin/      # 独立 sidecar crate（不依赖 tauri-build）
│       ├── miniterm-hook.rs      # Hook CLI 小工具（被 AI 工具 hook 调用）
│       ├── mt-ssh-cli.rs         # SSH CLI（终端 AI agent 经 Bash 调用；daemon 持久连接池）
│       └── mt-ssh-mcp.rs         # SSH MCP server（rmcp stdio；过渡期遗留通道）
├── relay-server/                 # 自托管中转服务（独立 Rust workspace）
│   ├── protocol/                 # 桌面端与中转共享的协议消息 crate（JSON over WebSocket）
│   ├── server/                   # axum 中转服务（只转发不落盘 + PWA 静态托管）
│   └── docker-compose.yml        # 一条命令从源码构建启动
├── mobile/                       # 移动端 PWA（React + TS + Vite，配对 / 列表 / 镜像 / 指令 / 发起会话 / 改名）
├── scripts/
│   ├── stage-sidecars.mjs        # 构建 sidecar 并按 triple 就位为 Tauri externalBin
│   └── stage-conpty.mjs          # 下载校验并就位固定版本 ConPTY 运行时（Windows）
├── tests/                        # Node 侧测试（ConPTY 打包 / TUI 滚动 / 布局恢复 / 主题兼容 / WSL 路径 / worktree 收敛等 20 个文件、100 条用例）
└── package.json
```

## 架构概览

### 数据流

```
用户键入 → xterm.onData → invoke('write_pty') → Rust PTY writer
Rust PTY reader → 16ms 批量缓冲 → emit('pty-output') → term.write()
进程退出       → emit('pty-exit')          → store.updatePaneStatusByPty('error')
进程监控 500ms → emit('pty-status-change') → StatusDot 更新
文件变更 notify → emit('fs-change')         → FileTree 刷新
ai-working → ai-idle(Stop)        → Toast + DONE Tag + requestUserAttention
attention 上升沿(PermissionRequest…) → Toast(警告色) + 提示音 + requestUserAttention
```

### Tauri 接口一览

- **Commands（70 个）** — PTY: `create_pty` · `write_pty` · `resize_pty` · `kill_pty`；FS: `list_directory` · `read_file_content` · `watch_directory` · `unwatch_directory` · `create_file` · `create_directory` · `rename_entry` · `delete_entry` · `filter_directories`；Search: `start_search` · `cancel_search`；Git: `get_git_status` · `get_git_diff` · `discover_git_repos` · `get_git_log` · `get_repo_branches` · `get_commit_files` · `get_commit_file_diff` · `git_pull` · `git_push` · `get_changes_status` · `git_stage` · `git_unstage` · `git_stage_all` · `git_unstage_all` · `git_commit` · `git_discard_file` · `list_worktrees` · `add_worktree` · `remove_worktree` · `prune_worktrees` · `get_worktree_branches`；Config: `load_config` · `save_config`；Editor: `open_in_editor` · `open_path_with_default_app`；Clipboard: `read_clipboard_image` · `save_clipboard_text`；AI: `get_ai_sessions` · `get_wsl_ai_sessions` · `get_ai_session_content`；WSL: `list_wsl_distros`；Hook: `register_ai_hooks` · `unregister_ai_hooks` · `get_ai_hook_registrations` · `get_hook_config_snippet` · `get_hook_status` · `toggle_hook_server`；SSH: `arm_ssh_autofill` · `prepare_ssh_key`；SSH 工具: `enable_ssh_tools` · `disable_ssh_tools`；SSH 远程: `ssh_remote_list_directory` · `ssh_remote_validate_dir` · `ssh_remote_ai_sessions` · `ssh_remote_ai_session_content` · `ssh_remote_upload_paste`；主题: `set_window_dark_mode`；移动端中转: `mobile_relay_apply` · `mobile_relay_status` · `mobile_relay_request_pairing_code` · `mobile_relay_reset_pairing` · `mobile_relay_update_sessions` · `mobile_relay_launchers_changed` · `mobile_relay_start_session_result` · `mobile_relay_check_launcher_command`
- **Events（12 个，后端 → 前端）** — `pty-output` · `pty-exit` · `pty-status-change` · `ai-user-submit`（AI 会话内用户按 Enter，用于打标记）· `fs-change` · `search-results` · `search-complete` · `wsl-shell-override` · `mobile-relay-status` · `mobile-relay-pairing-code` · `mobile-start-session` · `mobile-rename-pane`

### 状态优先级

终端面板状态从叶节点聚合到标签页和项目级别：

```
error > ai-working > ai-idle > idle
```

### 布局模型

```
App
├── ActivityBar（常驻最左侧图标栏：折叠中间栏 / Sessions / Git / 设置 / SSH / 移动端 + AI 状态角标）
└── Allotment 两栏（可拖拽，比例持久化）
    ├── 中间栏（可整栏折叠 · 纵向再分两块）
    │   ├── 上：ProjectList（项目 + 嵌套分组 + DONE 徽章）
    │   └── 下：FileTree（目录浏览 + Git 状态 + 文件操作）
    └── 右栏：TerminalArea × N（按项目常驻，仅活跃项目 display:block）
        ├── TabBar（标签管理 + ⚑ 标记下拉）
        └── SplitLayout（递归 SplitNode 分屏树）
            └── TerminalInstance × N（xterm.js + 右键菜单）

RightDrawer 从右边缘滑出、浮在终端之上（Sessions / Git 互斥单开，左缘可拖拽调宽并持久化）。
ToastContainer 悬浮于右下角，SettingsModal / SshModal / MobileRelayModal 覆盖全局。
```

## 推荐开发环境

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## 贡献

欢迎提交 Issue 和 PR。外部贡献会经过功能验证和安全审查后合并。

提交代码前请运行：

```bash
# 前端类型检查（tsc + vite build）
npm run build

# Node 侧测试（20 个文件、100 条用例）
node --test "tests/*.test.cjs"

# 桌面端 Rust 测试（565 个）
# 注意：mt-core / mt-ssh / mt-sidecars 是独立 crate 而非 workspace member，
# 单跑 `cd src-tauri && cargo test` 只覆盖 tauri-app 的 411 个，其余三个要分别指定 manifest。
cd src-tauri
cargo test                                        # tauri-app     411
cargo test --manifest-path mt-core/Cargo.toml     # mt-core        44
cargo test --manifest-path mt-ssh/Cargo.toml      # mt-ssh         26
cargo test --manifest-path mt-sidecars/Cargo.toml # mt-sidecars    84
cargo build

# 中转服务测试（53 个，独立 workspace）
cd ../relay-server && cargo test
```

## 社区

学 AI，上 L 站 — [LinuxDO](https://linux.do/)

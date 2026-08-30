<p align="center">
  <img src="icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>为 AI 时代打造的桌面终端管理器</strong><br>
  GPUI 原生 · 多项目 · 多标签 · 分屏布局 · AI 进程感知 · SSH 远程项目 · Git Worktree 管理 · 手机远程看 AI
</p>

<p align="center">
  <strong>完整功能清单 · 简体中文</strong> · <a href="features.md">English</a><br>
  <a href="../README.md">← 回到项目首页</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-1.1.7-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/GPUI-native-8A2BE2" alt="gpui">
  <img src="https://img.shields.io/badge/Rust-1.95%2B-dea584" alt="rust">
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
- **递归分屏** — 横向 / 纵向任意嵌套分屏，拖拽调整比例
- **pane 拖拽重排与合并** — 终端 tab 可整块拖走：拖到其它分组的 tab 栏或终端区中央即并入该组（tab 栏落点带插入位指示线，同组内拖动即前后换位），拖到终端区四边（1/4 进深）则在对应方向分出新屏，落点半透明高亮预览；拖拽走 GPUI 内建拖拽（on_drag / on_drop），Esc 中途取消，终端实例经缓存随布局树重排原样迁移，终端内容与 PTY 不受影响
- **pane 最大化** — 双击 tab 栏空白处（或右上角最大化按钮）把当前分组临时铺满终端区，再双击/点按钮还原；运行时状态不持久化，被最大化的 pane 关闭后自动回落整树视图，最大化期间发起分屏会先自动还原避免新屏不可见
- **高性能渲染** — alacritty_terminal 进程内 VT 解析 + GPU 原生渲染，零 IPC、零序列化；启用最小对比度，修复 Claude 提问文字在暗色下与背景近乎同色不可见的问题
- **滚动缓冲行数可调** — 主缓冲区保留行数可在设置里调整（默认 1 万行，改小当场生效并释放内存；历史版本曾因硬编码 10 万行在多项目多分屏叠加时把内存推向 OOM，教训记入了默认值），同时全局遵循标准 CSI 3J（ED3）；Codex 等应用可删除流式临时内容并重放折叠后的最终 transcript，`/clear` 也能真正清除旧历史。Windows 版内置并预载固定版本的官方 ConPTY 兼容运行时（资源校验失败时自动回退系统 ConPTY），让不同 Windows 版本下的 Codex 滚动与 transcript 折叠行为保持一致
- **终端缓存** — 切换项目 / 标签 / 分屏不重建终端实例，已有内容不丢失；启动按需懒加载，仅当前可见 pane 创建 PTY，避免历史项目终端越多启动越卡
- **项目切换缓存** — 文件树 / Git 历史数据按项目缓存，切回已访问项目零延迟渲染；目录加载与 Git 状态并行执行
- **复制粘贴** — `Ctrl+Shift+C/V`（macOS `⌘+Shift+C/V`）快捷键 + 右键菜单，未选中时"复制"自动置灰；可在设置中开启「智能 `Ctrl+C/V`」（有选区时 `Ctrl+C` 复制、无选区时中断程序，`Ctrl+V` 直接粘贴；macOS 上 `⌘` 系组合不受该开关约束）；Windows 大段多行粘贴自动分块写入，防止 ConPTY 丢行
- **拖选停留自动复制** — 拖选文本后按住鼠标静止超过设定时长（默认 1s，可调 0.2–60s，0 = 关闭）自动复制选区并在光标旁弹「已复制」气泡；松手时选区已继续增长则补复制一次，剪贴板始终是最终看到的完整选区
- **Alt / ⌥+单击定位光标** — 按住 Alt（macOS ⌥）单击终端里的某个格子，按与当前光标的列差合成左右方向键把光标挪过去（跟随 DECCKM 应用光标键模式，零位移不发、超过 512 步放弃、滚动回看时不生效）；**只在同一行内生效**，跨行一律不动——行编辑器里的上下方向键往往是召回历史而不是移动光标，宁可不动也不能毁掉正在输入的内容。bash / zsh / pwsh 提示符下逐格准确；Claude CLI 这类 Ink TUI 把硬件光标停在行末、起点对不上，不保证
- **长文本粘贴** — 剪贴板文本 ≥10 行或 ≥2000 字符时自动转存为临时 `.txt` 并粘贴带引号的文件路径，避免 AI 工具直接处理超长内容引发性能与 paste bracket 问题
- **图片粘贴** — 剪贴板含截图时自动检测，Windows 经 Win32 剪贴板 API（`CF_DIB` / `CF_BITMAP`）保存为临时 PNG 并粘贴带引号的路径，兼容 PinPix 等非标准格式；其余平台读系统剪贴板里的 PNG/JPEG 等原始字节直接落盘；图片确实在场却解不出来（如 `BI_BITFIELDS` 压缩位图）时发送 `Alt+V`，交给终端里的 AI 工具自行读取剪贴板
- **远程 / WSL 粘贴自动落地** — 上面两种「转存成文件再粘路径」的能力在远程终端里会自动换算落点：SSH 远程项目经 SFTP 把文件上传到远端目录后粘贴**远端**路径（默认 `<项目根>/.mini-term/pasted`，落在项目内 agent 无需额外授权即可读，目录可在设置中改成 `/tmp/mini-term`、`~/uploads` 等，并自动写入自忽略的 `.gitignore` 以免弄脏 `git status`）；WSL 项目则把 `C:\...` 换算为 `/mnt/c/...`（无需上传）。上传失败会明确弹提示，而不是粘一个远端读不到的本机路径
- **文件拖拽** — 文件树或系统资源管理器拖文件到终端自动插入带引号的绝对路径，精准定位目标分屏 pane，兼容含空格的路径；拖拽途中按 `Esc` 就地取消，路径不写入 PTY（Esc 被拖拽层吞掉，不会当成 `\x1b` 送进终端），松手也不会退化成一次普通点击把文件打开，悬停指示同步撤掉；只有真正进入拖拽后才吞 Esc，别处的 Esc 照常生效
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

- **WSL 目录作为项目根** — 支持把 `\\wsl$\<distro>\<unix-path>` 与 `\\wsl.localhost\<distro>\<unix-path>` 两种形式的 WSL 路径添加为项目，界面展示路径自动剥掉 `\\?\UNC\` verbatim 前缀，文件树可正常展开与预览
- **自动 wsl.exe 启动** — 检测到 cwd 是 WSL UNC 路径时，创建 PTY 忽略用户配置的 shell（cmd / pwsh 等），强制改用 `wsl.exe -d <distro> --cd <unix-path>` 启动，cwd 真正落在 WSL 里（`pwd` 显示 `/home/<user>/proj` 而不是 `C:\Windows`），与 Windows Terminal `MangleStartingDirectoryForWSL` 行为一致；distro 名从路径直接 parse，不调 `wsl -l -v` 探测；触发重写时弹一次性 toast 提示
- **已知限制** — WSL VM 内进程的 AI 状态识别能力受限，AI 状态可能失效；`notify` 文件监听在 WSL 9P 文件系统上事件大概率丢失，文件树需要手动刷新。仅 WSL2 验证，WSL1 兼容性未保证

### 文件搜索

- **全局搜索** — `Ctrl+Shift+F`（macOS `⌘+Shift+F`）快捷键或文件树工具栏按钮唤起，支持文件名搜索和文件内容搜索两种模式
- **路径匹配** — 文件名模式下查询串含 `/`（Windows 反斜杠亦可）时改为对相对项目根的路径匹配，如 `pages/task/my` 命中 `src/pages/task/my/my.vue`，高亮落在路径上；不含分隔符仍只匹配文件名
- **正则匹配** — 可切换子串 / 正则模式，结果关键词高亮显示
- **流式推送** — 后端使用 ignore crate 遍历文件树，每 50 条或 100ms 批量推送结果，支持随时取消
- **内容分组** — 内容搜索模式按文件分组展示匹配行号，点击结果直接预览并定位到匹配行

### AI 进程感知

- **Hook 事件系统** — 接入 Claude Code / Codex / Grok Build 官方 Hook API，接收 AI 工具事件（SessionStart / End、ToolUse 等），比进程轮询更精准及时；内置 `miniterm-hook` CLI 工具供 Hook 系统调用，自动 POST 事件到本地服务器；设置界面按「注入目标」勾选注册 / 卸载 Hook 配置——Claude Code / Codex / Grok 三家各一行可选，注册与卸载只作用于所选（三份配置文件互不相干，只用其中一家的用户没理由被写另外两家的配置）；每行显示该家的配置文件路径与注册现状（未注册 / 已注册 N 个事件 / 旧版本 N⁄M，黄色提示重新注册可补齐新增事件），默认勾选已经装了的那几家（老用户再点注册就是纯补齐），一家都没装过时全选保住首次一键注册的体验；写入合并而非覆盖用户已有 hook。Codex 权限请求从审批到工具执行完成期间持续保持 `ai-working`，避免提前触发任务完成提醒
- **实时状态检测** — Hook 一旦接入即为该面板的状态来源，逐轮状态直接由 hook 事件决定，不看输出活跃度（AI 空闲期 TUI 的定时重绘曾被误判为「又在工作」，导致完成通知反复触发）；无 hook 的面板降级为输入检测（识别键入的 `claude` / `codex` / `opencode` / `pi` / `grok` 命令，含 ↑ 历史与 Tab 补全的行快照兜底）加 500ms 输出活跃度轮询，显示 idle / working / error 状态
- **Grok Build 的 hook 接入** — `grok`（xAI 的终端 agent）走与 Claude / Codex 同一套 hook 链路，状态徽章、完成播报、AI 启动器与移动端发起会话全通。三处结构性差异各有对策：① grok 默认还会扫描 `~/.claude/settings.json` 的 hooks（Claude 兼容层），同一事件因此会来两趟——sidecar 按 `GROK_SESSION_ID` 加「有没有 argv」判出兼容层那趟并丢弃，而用户只注册了 Claude 时又必须放行（那是唯一来源），判据落在「原生 hook 文件是否在场」上；② 注册进 `~/.grok/hooks/` 的命令是**不含空格的裸文件名**（注册时把 hook 二进制复制进同目录），因为带空格的命令会被 grok 丢给 shell，而 Windows 上具体是 git-bash / pwsh / powershell / cmd 由环境决定、四家引号语义互斥，事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递；③ grok 没有 `PermissionRequest` 事件，「等你批准」是 `Notification` 的 `permission_prompt` 类型，归一化后点同一盏黄灯，而它的 `task_complete` 是知会不是待办，不点灯。另有一处专门抹平：grok 在会话收尾时会补发一次 `Stop`（`reason` 为 `channel_closed` / `shutdown`），不拦掉的话每次退出 grok 都要白响一声「任务完成」
- **Grok 的会话记录形态** — 与另外两家「一个文件一个会话」不同，grok 一个会话是**一整个目录**：`{grok_home}/sessions/{URL 编码的 cwd}/{session-id}/`，正文在 `updates.jsonl`（ACP 会话更新流），元信息在 `summary.json`。定位项目走**解码目录名**而不是编码项目路径（后者要逐字复刻它所用编码库的转义集；超长路径退化成 `{slug}-{hash}` 形态时回落读目录内的 `.cwd`）。正文一条消息会被拆成任意多个 chunk 行流式落盘，必须攒到边界（工具调用、回合收尾、对方开口）才算一条，否则一句回答在镜像里会碎成几十条。用量取 `turn_completed` 自带的 usage（按模型分解，ACP 口径的输入含缓存读写，拆成互斥桶后与 `totalTokens` 对齐）；**工具排行对 grok 为空**——持久化的 ACP `tool_call` 只带人类可读的 title，真正的工具名不落盘，拿 title 顶替会往排行里灌自然语言标签
- **只靠输入检测识别的 agent** — `opencode` / `pi` 没有接 hook，也没有可解析的本地会话记录：状态徽章、完成播报、AI 启动器与移动端发起会话四条链路照常可用，但对话镜像、AI 历史面板与用量统计对它们为空。镜像的启发式绑定据此设了白名单（`mt-relay::mirror` 的 `agent_has_session_log`），不在名单内直接返回空镜像，不会退而绑到同项目里其它 agent 最新的会话文件、把别人的对话贴到这个 pane 上。命令匹配走 basename 全等，`pip` / `ping` / `pixi` / `pi.py` 不会被误判成 `pi`
- **徽章卡死的三重兜底** — `Stop` 事件在若干情形下根本不触发：回合因 API 错误结束走 `StopFailure`（映射 ai-idle 并点黄灯提示回来重发）、用户按 Esc / Ctrl+C 打断则不发任何事件（由输入检测收敛，cause=`Interrupt`）；两者都覆盖不到的残余情况再由**停摆判定**兜底——hook 状态停在 ai-working 且状态与 PTY 输出双双静默 10 秒即收敛，此前已触发过退出（Ctrl+D / 双击 Ctrl+C / `/exit`，且之后无 hook 事件扶正）则判为已退出回落 idle，否则降为 ai-idle。三条兜底的结论都**一次性落盘**进 hook 状态，触发一次即收敛不再摆动，且 cause 一律不是 `Stop`，因此不会被当成「任务完成」播报（这正是 v0.9.3 删掉无记忆版兜底的原因）；正等用户批准的面板（如 Codex 的 `PermissionRequest`）豁免停摆判定，否则会连托盘黄灯一并抹掉
- **状态聚合** — 面板 → 标签页 → 项目逐层聚合，优先级 `error > ai-working > ai-idle > idle`
- **完成提醒三件套** — AI 任务从 working → idle、且成因确为 `Stop` 事件时立刻触发（权限请求、通知、澄清同样落到 `ai-idle`，不再被误报为任务完成；无 hook 的降级路径仍以下降沿为准）：
  - 右下角 Toast 桌面通知（仅非活跃项目弹出，同项目去重）
  - 项目列表 DONE 徽章，点击清除
  - 任务栏闪烁（Windows）/ Dock 跳动（macOS），窗口失焦时才触发
  - 提示音播放（内置合成默认音，支持自定义音频文件）
  - 所有通知开关独立可配，在「设置 → AI → 通知提醒」页统一管理（Hook 注册另在同组的「Hook 事件」页）
- **待确认提醒** — AI 停下来等你批工具权限、填 MCP 表单，或这一轮因 API 错误结束（`PermissionRequest` / `Elicitation` / `StopFailure`，与项目行黄灯同一判定）时，走上面同一套通道再提醒一次，开关独立、默认开（「设置 → AI → 通知提醒 → 触发时机」；它的触发频率远高于完成，只想留完成通知的人得能单独关掉）。判据取黄灯的**上升沿**而非「本次成因属待确认类」：后端把这类事件显式排除在去重之外（同一轮里第二次授权请求不能被吞掉），按成因判会一次待确认响好几声；黄灯亮着期间不重复提醒，你对该终端键入即视为已在处理（黄灯清除），下一次请求才重新构成上升沿。Toast 用警告色 + 感叹号与绿色的「已完成」区分，不设 DONE 徽章（那是完成态的标）
- **托盘状态灯** — 系统托盘常驻全局 AI 状态灯：黄=待确认、蓝=处理中、绿=完成未读、灰=安静，多状态并存且窗口失焦时轮播展示；右键托盘菜单列出**所有进入 AI 会话的项目**及各自状态（含 ⚪ AI 空闲待命的，不只列有动静的；排序 待确认 > 处理中 > 已完成 > 空闲，条数上限可配，空闲只进菜单不点灯）、点某项即定位到该项目内最该处理的那个 pane，左键唤起主窗口并跳到「下一个该我处理」的会话（与标题栏状态灯同一套落点，可在设置里关掉只唤起窗口；Linux 下仅右键菜单可用）；Notification 判定只认权限 / 确认类文案，API 错误与重试等待不点黄灯；可在设置中关闭
- **会话自动续接** — 重启后每个分屏 pane 自动写入 `claude --resume` / `codex resume` / `grok --resume` 续回上次会话：会话身份由 hook 上报、随布局持久化，跨一次重启保留；写入终端前经白名单校验（仅字母数字与 `-_`、长度上限 128），远程 pane 不参与，识别不了的一律不写；可在「设置 → 系统 → 常规」关闭（关掉后终端照常恢复，只是不自动跑续接命令）
- **会话进出检测** — 命令 echo 识别进入 AI；双击 `Ctrl+C` / `Ctrl+D` 或 `exit` / `quit` / `:quit` / `/logout` 识别退出
- **会话历史** — 读取本地 Claude / Codex / Grok 历史会话记录，右键复制恢复命令快速续接；首屏仅渲染 20 条，底部「加载更多」按钮按需展开（不再滚动即触发）
- **会话分支** — 把「在同一任务上并行试多条思路」做成一等公民（设计: `docs/plans/2026-08-14-session-branch-tree-design.md`）。**分支动作**：pane 右键「分支会话到新分屏」——原会话原地继续跑，右侧分出的新 pane 写入 fork 命令（Claude `--resume {id} --fork-session`，Codex `codex fork {id}`；命令模板走能力位表，sessionId 白名单校验，新接一家 AI 只需声明能力位；新 pane 是新进程，「本会话允许」的权限授权不迁移）。新 PTY 启动目录优先取会话记录 cwd（`claude --resume` 只认启动目录的会话桶）。**分支树**：历史面板「平铺|树」切换（持久化），fork 会话按缩进连线挂在父会话下——链路来自 CLI 亲写的磁盘指针（Claude：jsonl 复制行的 `forkedFrom.{sessionId,messageUuid}`，消息级；Codex：`session_meta.payload.forked_from_id`，会话级，按 `thread_source=="subagent"` 过滤掉子 agent 线程），mini-term 自己发起的 fork 另有**自记账**兜「会话文件未落盘的窗口期」（合并按 child 去重、磁盘优先）；悬空父落为根、环防御，树构建为纯逻辑、单测直测。**节点点击**：会话已有 pane 在跑 → 切项目激活聚焦；没开 → 新终端自动 resume（WSL/远程来源提示不可本机恢复）。pane 右键菜单的「查看会话分支」**悬停即展开**家族树面板——当前家族全貌 + 「← 当前」标记。树与面板的**厂商图标按会话最新使用的模型**显示（后端 64KB 尾窗反扫最新模型名过厂商推断规则，claude CLI 挂 GLM/DeepSeek 中转时亮真实厂商 icon，识别不出回落 CLI 图标；pane tab 的 CLI 图标刻意不变）。Grok 预留：无 CLI 级 fork，能力位缺席即菜单隐藏
- **会话查看** — 右键「查看」展示完整对话内容，User 纯文本 / Assistant Markdown 渲染，支持 `Ctrl+F` 搜索高亮和 User 消息快速导航
- **WSL 会话** — Windows 下直接读取 WSL 发行版内的 Claude / Codex 历史会话（不 spawn `wsl.exe`，走 `\\wsl$` UNC + 注册表枚举发行版）：WSL 根项目自动推导发行版与路径零配置加载；Windows 路径项目右键「WSL 会话」子菜单选择发行版后按 `/mnt` 规则映射扫描，靠会话内 cwd 精确校验防串项目；WSL 会话与本机会话按时间混排并带 WSL 标识，加载中头部显示 spinner，查看正文同样支持
- **AI 任务标记** — AI 会话内每次用户按 Enter 自动在终端打点，标签右上角 ⚑ 按钮下拉展示历史提交列表，点击或 `Ctrl+Shift+↑/↓`（macOS `⌘+Shift+↑/↓`）在标记间跳转，目标行短暂高亮提示

### 使用统计

- **多维聚合面板** — 顶栏「统计」打开：Claude Code / Codex / Grok 的成本、调用次数、会话数三组 KPI，按日 / 按小时趋势图（自绘渲染），模型排行、项目排行与 Top 会话；agent / 时间范围 / 项目过滤随手切换，自定义起止日期带日历选择器（自绘，范围钳在近一年内）
- **面板交互细节** — 项目过滤下拉贴着触发按钮弹出并按视口封高，项目再多也顶不出屏幕，超出部分带一条可拖的滚动条；刷新按钮悬停 500ms 出提示。滚动条只给这类「调用方封了高」的下拉式菜单，右键菜单一律不配——它们十来项本就滚不动，套上只会白多一条轨道与让位边
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
- **悬停 pane 预览** — **仅限跑着 AI 会话的项目**（判定与项目行 AI 品牌图标同口径：行上亮着图标才有预览；AI 退出后浮层随即收起，普通 shell 项目悬停只出绝对路径 tooltip，不弹卡打断视线）。悬停项目行 250ms 弹出该项目终端区的**微缩布局拼图**：按 SplitNode 树复现真实分屏比例，浮层固定宽度永不超屏，与切过去看到的所见即所得；打开期间 500ms 重画，预览是活的。实现为读取终端 grid 自绘微缩位图——与主终端同一条渲染链路取格子内容（同色 run 提取、粗体标准色亮化、256 色 / truecolor 解析），按 cell 网格绘制位图再等比缩放，隐藏 pane 的内容照样实时可得。每个分屏叶子显示当前 tab 的画面（左下锚定，保住最新输出与 TUI 输入区），隐藏 tab 以「+N」徽章示数并附其中最高优先级的状态点（error > ai-working > ai-idle，与状态聚合同口径）——藏在非激活 tab 里的 AI 状态不漏报；未起 PTY 的 pane 显示「未启动」占位（项目绝对路径在卡头可见）。**非激活的 pane tab** 悬停 250ms 同样弹单格缩略图浮层（同一渲染链路，打开期间 500ms 重画；未启动占位与远程断线遮罩同口径），且**不做 AI 开闸**——隐藏 tab 的内容不切过去本来就看不见，预览回答的就是「那个 tab 里现在是什么」。触发时序与项目行预览同一套，移出/点击/右键/滚动即关；卡片钳制左右边界，底部分屏放不下时翻到 tab 上方
- **拖拽添加项目** — 从资源管理器拖拽文件夹到项目列表即可快速添加，自动识别文件 / 文件夹 / 重复项目并给出视觉反馈
- **嵌套分组** — 最多 3 级项目分组，拖拽排序，折叠 / 展开，分组右键菜单可直接添加本地项目或远程 SSH 项目并归入该组（折叠的分组自动展开）；「删除分组」先弹确认并说明组内项目会移到上一级而非被删除；「移动到分组」按分组树逐级展开子菜单，当前所在组标 ✓ 并置灰，超深度的组不可选
- **Worktree 子项目** — worktree「设为项目」后挂在主项目下方作子项目（缩进跟随分组），拖出或右键「脱离父项目」可转回顶层，删除父项目时子项目原位晋升不丢失；项目列表为 worktree 项目显示 ⎇ 分支徽章，仓库列表与 Changes 下拉同样标注 worktree 条目；**外部删除的 worktree 自动收敛** —— 窗口重获焦点时探测子项目目录是否还在，AI agent 在终端里跑完 `git worktree remove` 后，目录已消失的子项目连同终端资源一并移除，⎇ 徽章同步重探（仅在父项目目录仍存在时清理，盘符掉线不会误删；SSH 远程与 UNC/WSL 路径不参与），worktree 弹窗「清理失效条目」也会一并移除指向它的项目
- **文件树** — 集成目录浏览器，自然排序（V1 → V2 → V10 而非字典序），嵌套 `.gitignore` 置灰（每层子目录的忽略规则与 `!pattern` 白名单都会生效，与 git 行为一致），`notify` 文件监听实时刷新
- **文件操作** — 文件树内新建文件 / 文件夹、重命名、删除、查看内容（Markdown 渲染，图片格式直接展示，二进制与超大文件友好提示）
- **文件工作区** — 文件树点开的本地 / 远程文件在主区页签中查看、编辑、保存，与终端并列切换：tree-sitter 语法高亮（30+ 语言）按文件类型自动匹配，基础缩进，查找替换（`Ctrl+F`）；`Ctrl+S` 原子落盘（临时文件 + rename，不怕写坏），CRLF 文件按原行尾往返不产生全文件 diff；有未保存修改时关闭先确认，文件被外部改动时干净则静默重载、脏则出提示条；Markdown 预览实时渲染未保存草稿；语法配色跟随主题皮肤。远程文件经 SFTP 读写：保存前比对加载时的基线，远端被别人改过就提示重载或强制覆盖，写入走临时文件 + 备份 + rename，陈旧备份自动清理；刷新失败只出横幅不盖住编辑器；远程文件也可直接下载到本地
- **文档预览里的图片** — Markdown 与 HTML 预览都能显示图片：相对路径按当前文件所在目录解析成本地资源，「整行只有图片」的行拆出来自绘，宽度取图片原尺寸与正文可用宽的小值（大图不再被 object-fit 压成一条）；SVG 按 2 倍光栅化换算。网络图片（README 顶上的徽章、外链截图）经内置 HTTP 客户端真加载——只放行 `file://` 与 `http(s)://`，10s 超时 + 32MB 响应上限，客户端为进程级单例；拉不动时画成带 alt 的可点占位，点了用系统浏览器打开原图。远程文件的 Markdown 属不可信输入，按渲染器同一份 GFM AST 清洗并迭代到不动点：原始 HTML 整体按源码显示，链接只放行 http(s) / mailto / tel / 锚点，图片不内联加载（整行图片点击后才拉取）；远程 HTML 不进预览、只看源码
- **HTML 预览** — `.html` 除源码编辑器外另有预览态（简版渲染，顶部一条「无 CSS / 无脚本」说明），`src` / `href` / `poster` 的本地目标改写成 `file://` 才看得到图片与本地资源；工具栏常驻「用浏览器打开」，走 **https 协议关联**而非 `.html` 文件关联（后者常被设成编辑器，点了只会再开一个编辑器）——Windows 读 `https` 的 UserChoice ProgId 再取 `shell\open\command`，三层退让 https → http → 系统级 `HKCR\http`，找不到浏览器直接报错而不悄悄退回文件关联；路径转 URL 时转义 `%`、空格、`#`、`?`
- **外部编辑器打开** — 文件树右上角按钮一键用配置的编辑器（默认 VS Code）打开当前项目，路径可在「设置 → 系统 → 外部编辑器」自定义；文件可用系统默认应用打开
- **项目级环境变量** — 项目右键菜单「环境变量…」打开管理弹窗，行级 `[启用 checkbox][key][value][✕]` 布局，启动该项目终端时按项目注入到 PTY 子进程；严格 POSIX 校验（key 匹配 `^[A-Za-z_][A-Za-z0-9_]*$`、非 `MINITERM_` 前缀、不可用 `WSLENV`、项目内不重复，value 禁 `\n/\r/\0`）；校验之外再加 `MINITERM_` 前缀 + `WSLENV` 防御性过滤，即便手改 `config.json` 绕过 UI 校验也无法破坏 hook 协议或 WSLENV 拼接；WSL 项目下环境变量通过 WSLENV 机制透传至 Linux bash（`/u` 单向不做路径翻译；`~/.bashrc` 中 `export` 同名变量会覆盖）

### Git 集成

- **文件状态** — 文件树显示 Git 状态颜色（修改 / 新增 / 删除 / 冲突）
- **变更 / 历史同屏** — Git 面板为上下两个可折叠区块：更改在上、提交历史在下，中缝可拖拽调节比例（钳 15%~85%），折叠 / 展开带动画且会话内记住折叠态与比例；面板顶部仓库栏下拉切换仓库（worktree 条目标 ⎇），分支徽章点击只切历史查看分支（不 checkout，查看非 HEAD 分支时高亮提示），刷新 / Pull / Push 集中在栏上，右键仓库名可在终端打开或进入 Worktree 管理
- **变更 Diff** — 工作区文件变更的详细 Diff，Hunk 行级解析，并排 / 内联双视图，并排模式支持拖拽调节分隔比例，字号跟随终端字体设置。单文件与 commit 两个 diff 弹窗统一 80vw × 85vh（与用量统计面板外框重合）；长行横向滚动、并排两栏纵向同步、`@@` hunk 头分隔与「上一处 / 下一处改动」跳转；LCS 回溯的平局偏向已修（此前删 / 增行永远配不上对、左右各占一行错开），配对成功的行再做词级高亮只涂真正变了的片段；diff 规模判据按剥掉公共前后缀后的中段算，几千行文件改一行不再退化成「整块替换」
- **提交历史** — 平铺展示顶部仓库栏选中仓库的提交记录，游标分页加载（默认 30 条）
- **分支拓扑图** — 提交历史每行左侧绘制 SVG 拓扑图，按 lane 布局画出分支、合并与直穿连线，节点按 lane 上色、合并提交实心点套外环，汇入线用分支自身颜色的贝塞尔曲线并在根部渐变融入主线；后端 revwalk 追加 TOPOLOGICAL 排序，避免时钟偏移或 rebase 后父提交排在子提交之前导致连线断裂；commit 行只标注本仓库自己检出的分支，不再把其他工作区 / 远程分支全挂上来
- **提交 Diff** — 查看任意提交的文件变更，逐文件切换
- **分支信息** — 本地 / 远程分支列表
- **源码控制面板** — VS Code 风格 Changes 面板，Staged / Changes / Untracked 分组展示，支持单文件和全量 stage / unstage / discard，`Ctrl+Enter` 快速提交，列表与树形视图切换
- **Pull / Push** — 顶部仓库栏按钮一键同步远端，刷新按钮重新加载提交记录与分支信息
- **多仓库发现** — 自动扫描项目目录下所有 Git 仓库（递归 5 层，跳过 `node_modules` 等）
- **Worktree 管理** — 项目右键菜单或 Git 面板顶部仓库栏右键打开「Worktree 管理」弹窗：列出全部 worktree、基于现有分支或新建分支创建、删除（可强制）、清理失效条目，增删后即时刷新仓库列表；worktree 可一键「设为项目」或直接在终端打开，pane 支持工作目录覆盖并随布局持久化、分屏继承目录。项目根目录本身不是仓库时会向下扫描子仓库，按主工作区归并为分组列表，组头可勾选多选 / 全选，一次为每个勾选的仓库各建一个 worktree（分支下拉取各仓库分支交集，路径框语义变为父目录并预览 `<仓库名>-<分支>` 落点，失败的逐仓库列出错误）

### 外观与配置

- **图标侧栏 + 三栏布局** — 最左侧常驻图标栏（折叠中间栏 / Sessions / Git / 设置 / SSH）；中间栏纵向叠放 Projects 与 Files、可整栏一键折叠；右侧为终端。Sessions / Git 改为从右边缘滑出、浮在终端之上的悬浮抽屉（互斥单开，左缘可拖拽调宽并持久化，✕ 关闭），激活态蓝色竖条指示
- **三种主题模式** — Auto（跟随系统）/ Light / Dark，深色基于 Warm Carbon 暖炭色调；标题栏由应用自绘、配色跟随主题，启动深色用户无首帧浅色闪烁
- **自定义标题栏** — 无边框窗口改由应用自绘顶栏，左侧应用名、版本号、项目切换器与全局状态灯，右侧窗口控制，配色跟随主题而不再是系统那条灰白。按平台适配窗口习惯：
  - **Windows / Linux** — 最小化 / 最大化 / 关闭三键靠右，关闭键悬停变红。Win11 的**贴靠布局**照常可用：悬停最大化按钮即弹分屏菜单
  - **macOS** — 保留系统原生交通灯，左上角留出让位，不自绘三色圆点，全屏 / 手势 / 系统集成一并保住；关闭最后一个窗口即退出进程，不会在 Dock 里留下一个点不开的僵尸图标
  - **项目切换器** — 版本号右侧以竖线隔开的胶囊按钮，常显当前项目名与它自己的 AI 状态色点（没有 AI 会话时压暗）；下拉列出所有进入 AI 会话的项目及状态（与托盘右键菜单同一份聚合，按 待确认 > 处理中 > 已完成 > 空闲 排序），点击项目即切换并定位到该项目内最该处理的 pane，全都安静时只切换项目
  - **全局状态灯** — 紧挨项目切换器右侧，汇总所有项目所有 pane 的最紧急一档（异常 > 待确认 > 处理中 > 已完成），点击跳到「下一个该我处理」的会话：待确认 / 异常优先，其次是**最先完成**的那个，最后才是还在跑的。与托盘右键菜单的排序有意不同——托盘回答「哪些项目还活着」，状态灯回答「下一件该做什么」
  - 拖拽与窗口控制走 GPUI 原生 WindowControlArea；双击顶栏最大化 / 还原
- **外置主题包（Dream Skin 兼容）** — 「设置 → 外观 → 主题与语言」可从文件夹或 zip 导入第三方皮肤，落在 `{app_data_dir}/themes/<themeId>/`（`theme.json` 必需，`theme.css` / 背景图可选）。同一区的「生成示例」把一份可直接改的示例皮肤写进 `themes/example/`（`theme.json` + `theme.css` + 逐字段说明的 `README.md`，改完保存即热重载）；示例内容与仓库 [`docs/theme-pack-example/`](theme-pack-example/) 是**同一份文件**（`include_str!` 编译期嵌入，文档与产物不会漂开），目录已存在时报错而非覆盖，用户改过的那份不会被静默抹掉。包内带 `manifest.json` 时逐文件核对 bytes + sha256 防损坏；导入先落暂存目录、校验通过才原子换入，坏包不会连累同名的既有皮肤。皮肤的明暗由作者在 `theme.json` 的 `appearance` 定死，激活期间内置主题按钮置为未选中态。改动包内文件即热重载。皮肤可声明背景图，此时终端底色转半透明压在氛围层上，设置页卡片直接铺实况缩略图。导入的 `theme.css` 与 `theme.json` 的 `tokens` 覆盖过同一道外链闸：禁 `@import`、指向包外的引用一律拒 —— 检查在剥掉注释、还原 CSS 转义后的取样上做，`url()` 与 `image-set("…")` 这类裸字符串双查，`url(\68 ttps://…)` 之类的转义写法同样挡得住
- **字体独立调节** — UI 与终端的字号（10-20px）/ 字体 family 分别可调，终端可选是否跟随 UI 主题；默认字族按平台选择——Windows Cascadia Mono、macOS Menlo、Linux DejaVu Sans Mono，各自带 CJK 与 emoji 回退，主字体缺席时不会回落成比例字体
- **终端连体字** — 「设置 → 外观 → 字体」的「启用终端连体字」开关（默认关），开启后 `=>` `!=` `->` 等按字体自身的连字规则合并显示。合并段整段一次 shape、段原点钉在 `cell_width × 起始列`，连字总宽守恒时段内字符照旧落在列格上；shape 完若总宽不等于「列数 × 列宽」则退回禁连字重 shape 一次，防住连字不守恒的字体。注意默认字族 Cascadia **Mono** 是去连字版，要换 Cascadia Code / Fira Code 这类才看得见效果
- **布局持久化** — 分屏比例、标签页、窗口大小 / 位置自动保存，重启恢复
- **关闭确认** — 关闭窗口时只按 AI 会话数量盘点（ai-working / ai-idle 的 pane），裸 shell 终端不计入，仅当存在 AI 会话时才弹确认并列出会话名清单；无论是否弹窗都会 flush 所有项目布局
- **版本检查** — 启动时拉取 GitHub Release，有新版本时侧栏图标高亮提示、点击前往下载；版本号写入原生窗口标题
- **中英双语界面** — 「设置 → 外观 → 主题与语言」一键切换中 / 英文，整个界面实时重渲染；首次启动按系统语言自动探测并记忆选择，重启保留。每个页面、每个功能的文案均已翻译，内置轻量 i18n 层（无额外运行时依赖）
- **设置中心** — 统一的设置面板，侧栏为「分组 + 分页」两级菜单：终端（Shell / 复制粘贴）、外观（主题与语言 / 字体）、AI（通知提醒 / Hook 事件）、系统（常规 / 外部编辑器），快捷键与关于留在顶级。按主题归组后每页只剩一屏左右，不再出现「一页塞九组控件、找个开关要滚半页」的老问题
- **满屏图标体系** — 文件树文件类型 / 文件夹图标（含目录展开态），项目行 AI 品牌图标与技术栈图标——官方品牌 SVG 形状，原生自绘渲染
- **启动性能** — 原生渲染无 Web 资源，启动路径零网络请求、离线首帧不受影响（价格表按天拉取，拉不到用缓存）；统一时间轴启动埋点写 stderr，便于回归定位
- **界面动效** — 弹窗 / 右键菜单 / 侧拉抽屉共用一套进出场动画：遮罩淡入、面板落下并放大到位，关闭时反向播完再卸载（期间冻结内容，不会在淡出中变空或仍吃 Esc）；右键菜单从光标位置展开，切换终端与新建分屏各有过渡。系统关掉窗口动画时这套转场照常保留，用量统计面板的数字滚动与图表补间同样豁免，只停掉状态点闪烁一类的循环动画

## 技术栈

整套应用为 **Rust 原生实现**（早期 Tauri + React 版已移除，源码看 git 历史）：

| 层 | 实现 |
|---|---|
| 壳 / 渲染 | GPUI 0.2（Zed 同源框架，GPU 原生渲染，单进程、无 WebView） |
| UI | 纯 Rust：gpui-component + 自绘组件 |
| 终端 | alacritty_terminal（进程内 VT 解析，零 IPC、零序列化）· portable-pty |
| 状态 / 布局 | 单一 Store · 递归 SplitNode 分屏树 |
| Git / 文件 | git2（libgit2）· notify + ignore |
| 用量统计 | rusqlite 本地账本 · 自绘趋势图 |
| 移动端中转 | axum + tokio WebSocket（`relay-server/`）· React + Vite PWA（`mobile/`） |
| 测试 | **1672 个 Rust 测试**（28 个测试目标）+ 中转服务端协议边界测试 |

## 快速开始

### 直接下载

前往 [Releases](https://github.com/dreamlonglll/mini-term/releases) 下载，三平台产物：

- **Windows x64（主要支持平台）** — `Mini-Term_*_x64-setup.exe` 安装包（NSIS，用户级安装免管理员，装过旧版的默认原目录原地升级）
- **macOS arm64** — `Mini-Term_*_aarch64.dmg`
- **Linux x64** — `Mini-Term_*_amd64.deb` 或 `Mini-Term_*_amd64.tar.gz`

> **平台支持说明**
> - **Windows** — 主要支持平台，保证可用性，日常开发与测试均在 Windows 上进行
> - **macOS / Linux** — 代码层面已支持，但**可用性欠佳**，未经充分打磨，欢迎提 Issue 反馈

#### macOS 安装提示

下载 `.dmg` 后双击打开,如果系统弹出 **"Mini-Term" is damaged and can't be opened. You should move it to the Bin**(已损坏,移到废纸篓),这并不是文件真的损坏 —— 而是 Release 产物没有 Apple Developer ID 签名,被 Gatekeeper 因 quarantine 标记拒绝。

把 `.app` 拖入 `/Applications` 后,在终端执行一次即可解除限制:

```bash
xattr -cr /Applications/Mini-Term.app
```

之后正常双击启动。每次升级新版本都需要再执行一次。

### 从源码构建

#### 前置条件

- [Rust](https://www.rust-lang.org/tools/install) >= 1.95
- [Node.js](https://nodejs.org/) >= 20 —— 仅 sidecar 就位脚本使用（纯标准库，无 npm 依赖）

#### 构建与运行

```bash
# 克隆仓库
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term

# 构建三个 sidecar 并连同便携 ConPTY 就位到 target/debug/
node scripts/stage-sidecars.mjs

# 开发运行
cargo run -p mt-app

# 发布构建（产物 target/release/mini-term(.exe)）
cargo build --release -p mt-app
```

> hook 上报与便携 ConPTY 按「与 exe 同目录」定位 sidecar 与资源，发布包已带齐；源码运行要完整体验，先跑一次 `stage-sidecars.mjs`（release 构建对应 `--release`，就位到 `target/release/`）。

## 项目结构

```
mini-term/
├── crates/                       # 主工作区（12 个 crate）
│   ├── mt-app/                   # GPUI 应用壳：Workspace 组件树、AppStore 全局状态、SplitNode 布局树、各面板 / 弹窗 / 托盘 / 标题栏
│   ├── mt-ui/                    # GPUI 渲染层：终端 view / element、主题桥（不含业务逻辑）
│   ├── mt-terminal/              # VT 状态机 + grid 模型（alacritty_terminal 封装，不依赖 gpui）
│   ├── mt-pty/                   # PTY 生命周期（spawn / read / write / resize / kill）+ 便携 ConPTY 预载
│   ├── mt-ai/                    # AI 感知：hook server（权威）、hook 注册、输入检测降级、状态判定、会话记录读取
│   ├── mt-project/               # 文件树、目录监听、搜索、Git（git2）、外部编辑器、WSL 发行版枚举
│   ├── mt-config/                # 配置持久化与主题包（不依赖 gpui）
│   ├── mt-i18n/                  # 双语文案层（字典源头 locales/*.ts，dict.rs 由生成器产出）
│   ├── mt-relay/                 # 移动端中转桌面侧：出站 WSS 长连、配对、项目快照 / 增量、对话镜像、指令写穿
│   ├── mt-ssh/                   # 共享 SSH 通信层（russh 持久会话池 + SFTP 原语，主程序与 sidecar 共用）
│   ├── mt-usage/                 # 用量统计：会话轮次解析 / SQLite 账本 / 聚合 / 计价
│   └── mt-core/                  # 叶子共享库（WSL UNC 解析 / SSH 提示扫描 / 原子写等）
├── sidecars/                     # sidecar 二进制独立工作区（版本号自成语义，不跟随主程序发版）
│   ├── miniterm-hook             # Hook CLI 小工具（被 AI 工具 hook 调用）
│   ├── mt-ssh-cli                # SSH CLI（终端 AI agent 经 Bash 调用；daemon 持久连接池）
│   └── mt-ssh-mcp                # SSH MCP server（rmcp stdio；过渡期遗留通道）
├── relay-server/                 # 自托管中转服务（独立 Rust workspace）
│   ├── protocol/                 # 桌面端与中转共享的协议消息 crate（JSON over WebSocket）
│   ├── server/                   # axum 中转服务（只转发不落盘 + PWA 静态托管）
│   └── docker-compose.yml        # 一条命令从源码构建启动
├── mobile/                       # 移动端 PWA（React + TS + Vite，配对 / 列表 / 镜像 / 指令 / 发起会话 / 改名）
├── scripts/
│   ├── stage-sidecars.mjs        # 构建 sidecar 并连同便携 ConPTY 就位到主程序 exe 同目录
│   └── stage-conpty.mjs          # 下载校验并就位固定版本 ConPTY 运行时（Windows）
├── tests/                        # Node 侧测试（2 个文件：ConPTY 打包 / vendored-openssl 守卫）
└── docs/                         # 文档（功能清单 / 中转部署 / 主题包示例等）
```

## 架构概览

### 数据流（单进程，无 IPC）

```
用户键入 → 终端 pane 写入 → AI 输入感知 → PTY 写入
PTY reader 线程 → 直喂 VT 状态机（alacritty_terminal）→ 唤醒重绘 → UI 按帧取 grid 渲染
hook 上报 / 500ms 轮询 → 状态判定 → AppStore → 状态点 / 托盘 / 项目列表
文件变更 notify → 文件树刷新
布局 / 配置变化 → 防抖 → 配置落盘
ai-working → ai-idle(Stop)          → Toast + DONE 徽章 + 任务栏提醒
attention 上升沿(PermissionRequest…) → Toast(警告色) + 提示音 + 任务栏提醒
```

单进程架构，无 IPC 接口层——原 Tauri 版的 16ms 批量缓冲、有界 channel、双水位背压与孤儿 PTY 回收整套机制是为 WebView IPC 边界造的，已随架构作废。

### 状态优先级

终端面板状态从叶节点聚合到标签页和项目级别：

```
error > ai-working > ai-idle > idle
```

### 组件树

```
Root（gpui-component 根，承载 Dialog / 通知层）
 └─ Workspace（持有 AppStore 与各栏视图）
     ├─ background_art（主题包背景图，窗口级）
     ├─ ActivityBar（44px 窄边条图标栏）
     ├─ h_resizable 三栏（可拖拽，比例持久化）
     │   ├─ 中间栏（可整栏折叠 · v_resizable 纵向再分两块）
     │   │   ├─ 上：ProjectList（项目 + 嵌套分组 + DONE 徽章）
     │   │   └─ 下：FileTree（目录浏览 + Git 状态 + 文件操作）
     │   ├─ TerminalArea（SplitNode 树 → 嵌套 resizable；leaf = tab 栏 + 终端 pane 实体）
     │   └─ SessionPanel（AI 历史，右侧抽屉，可折叠）
     ├─ UsagePanel（用量统计浮层）
     ├─ 弹窗层（设置 / SSH / 移动端等各类 Modal）
     └─ 通知层（完成 / 待确认 toast）
```

## 推荐开发环境

- [VS Code](https://code.visualstudio.com/) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## 贡献

欢迎提交 Issue 和 PR。外部贡献会经过功能验证和安全审查后合并。

提交代码前请运行：

```bash
# 全工作区 Rust 测试（28 个测试目标、1672 例）
cargo test --workspace

# Node 侧测试（仅 2 个文件：ConPTY 打包 / vendored-openssl 守卫）
node --test "tests/*.test.cjs"

# 中转服务测试（独立 workspace）
cd relay-server && cargo test
```

## 社区

学 AI，上 L 站 — [LinuxDO](https://linux.do/)

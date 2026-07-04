# PRD: Session 块支持查看 WSL 内 claude/codex 历史会话

## 背景

Session 块（`SessionList.tsx` → `get_ai_sessions`）目前只扫描 **Windows 宿主**的
`~/.claude/projects` 与 `~/.codex/sessions`。在 WSL 发行版内运行的 claude/codex，
其会话文件存于发行版文件系统内部（发行版用户 home 下的 `~/.claude` / `~/.codex`），
Windows 侧永远扫不到。用户需要在 Session 块中看到这些 WSL 会话。

术语见根目录 `CONTEXT.md`（会话来源 / WSL 会话 / WSL 根项目 / WSL 关联项目）。

## 需求决议（与用户逐条确认过）

1. **两种场景都支持**：
   - **WSL 根项目**（项目根是 `\\wsl$\...` / `\\wsl.localhost\...` UNC 路径）：
     自动启用，零配置。distro 与 Linux 侧 cwd 用现有 `mt-core::wsl_path::parse_unc` 从路径推导。
   - **WSL 关联项目**（项目根是普通 Windows 路径，用户在 WSL 内经 `/mnt/...` 对同一目录干活）：
     通过项目右键菜单勾选启用，并选择发行版。
2. **合并显示**：勾选/自动启用后，WSL 会话与 Windows 会话按时间戳降序混排在同一列表，
   WSL 来源的条目带标识（如时间行旁 "WSL" 小字）。Windows 侧扫描逻辑不变。
3. **distro 选择**：右键子菜单枚举已安装发行版。枚举读注册表
   `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss\{guid}\DistributionName`
   （`DefaultDistribution` 值指向默认发行版的 guid），**不 spawn wsl.exe**。
4. **WSL 内 home 定位**：枚举 `\\wsl$\<distro>\home\*` 加 `\\wsl$\<distro>\root`，
   凡含 `.claude` 或 `.codex` 目录的 home 都纳入扫描。防串项目由现有 cwd 精确校验兜底
   （claude: `dir_matches_project` 读 jsonl 内 cwd；codex: `session_meta.payload.cwd` 匹配）。
5. **VM 隐式拉起**：接受。直接读 UNC（若 VM 未运行 Windows 会自动启动它）；
   读失败/异常一律静默降级为空列表，不弹错。
6. **UI 入口**：项目右键菜单新增「WSL 会话」项，悬停展开子菜单：
   `[不启用] + <发行版列表>`，当前选中项打 ✓。**WSL 根项目不显示此菜单项**（自动生效）。
   已配置的发行版若枚举不到（被卸载），子菜单中显示为"<名字>（未找到）"标记项。
7. **resume 命令**：右键「复制 resume 命令」保持 `claude --resume <id>` / `codex resume <id>`
   原样，不加 wsl 前缀（用户在该项目的 WSL 终端里粘贴即可用）。
8. **分段加载**：前端并行发两个请求——现有 `get_ai_sessions`（Windows 侧，秒出先显示）
   与新的 WSL 会话 command；WSL 结果到达后按时间排序合并进列表。
   WSL 部分加载中时 Sessions 头部显示小 spinner。

## 实现细节（已拍板）

### 配置

- `ProjectConfig` 新增 `wslSessionsDistro?: string`（TS + Rust serde 同步）。
  `undefined` = 未启用；非空 = 启用且指定发行版名。WSL 根项目不落此配置。
- 旧配置无此字段，天然兼容。

### 路径映射（场景 A）

- `D:\Git\foo` → `/mnt/d/git/foo`：盘符转小写、`\`→`/`、挂在 `/mnt/` 下。
- **只支持默认 `/mnt` 挂载根**，不解析 `/etc/wsl.conf` 自定义 automount root（YAGNI）。
- cwd 匹配统一转小写比较（drvfs 默认大小写不敏感；`/home/...` 场景同一路径出现
  大小写变体目录的概率可忽略）。需要一个 Linux 语义的 normalize（保留 `/`、lowercase、
  去尾部 `/`），不能复用 Windows 版 `normalize_path`（它把 `/` 换成 `\`）。
- Claude 项目目录名编码复用现有 `encode_project_path`（对 unix cwd 同样成立：
  `/mnt/d/git/foo` → `-mnt-d-git-foo`）。

### 后端（src-tauri）

- 新 command `list_wsl_distros() -> Vec<WslDistro>`：
  `{ name: string, is_default: bool }`，读注册表枚举；非 Windows 返回空。
  （注册表访问可用 `windows-registry`/`winreg` crate，注意按现有 Cargo.toml 依赖习惯选择。）
- 新 command `get_wsl_ai_sessions(project_path, distro?) -> Vec<AiSession>`：
  - `project_path` 为 UNC（WSL 根项目）：`parse_unc` 得 distro + unix cwd，忽略入参 distro。
  - `project_path` 为 Windows 路径（WSL 关联项目）：必须给 distro，按 /mnt 规则映射出 unix cwd。
  - 扫描 `\\wsl$\<distro>\<home>\.claude\projects\<encoded>\*.jsonl` 与
    `\\wsl$\<distro>\<home>\.codex\sessions\...`（含 `session_index.jsonl` thread_name），
    解析逻辑与现有 `get_claude_sessions`/`get_codex_sessions` 同构，尽量抽公共函数复用而非复制。
  - **WSL 侧扫描上限下调**（9P 慢）：claude ≤100 文件、codex ≤200 文件；
    `MAX_SESSIONS_PER_SOURCE`/`MAX_TOTAL_SESSIONS` 语义保持。
  - 缓存复用 `SESSION_CACHE` 思路，cache key 掺入 distro；WSL 侧 TTL 放宽到 10s。
  - command 增加 `force?: bool`（或等价机制）供手动刷新绕过缓存；
    前端刷新按钮对 WSL 请求传 force。
  - 任何 IO 失败静默返回空 `Vec`。
- `AiSession` 新增可选来源字段（如 `wslDistro: Option<String>`，
  `#[serde(skip_serializing_if = "Option::is_none")]`），前端据此渲染标识并回传。
- `get_ai_session_content` 增加可选 `wslDistro` 参数：有值时从对应 UNC 位置定位会话文件
  读取正文（查看功能必须支持 WSL 会话）。
- 注意 spec: `tauri-command-nested-args.md`（command 参数命名/嵌套约定）、
  `windows-unc-verbatim-prefix-strip.md`（UNC verbatim 前缀）、
  `wsl-exe-cd-path-semantics.md`（WSL 路径语义）。

### 前端（src）

- `types.ts`：`ProjectConfig.wslSessionsDistro?`、`AiSession.wslDistro?`、`WslDistro`。
- `ProjectList.tsx` 右键菜单：非 UNC 项目追加「WSL 会话」子菜单；
  distro 列表异步获取并缓存（打开菜单前已就绪，可在 store 或模块级缓存，首次触发获取）。
  点击某发行版 → 写 `wslSessionsDistro` 并 `save_config`；点击「不启用」→ 置 undefined。
- `SessionList.tsx`：
  - 判定当前项目的 WSL 来源：UNC 根项目（`parse_unc` 语义在前端简单判 `\\wsl$`/`\\wsl.localhost`
    前缀即可，或后端判定）或 `wslSessionsDistro` 有值 → 并行请求 WSL 会话。
  - 两个请求各自到达各自 set state，渲染时合并排序（按 timestamp 降序）。
  - WSL 条目标识：时间行追加 "WSL" 灰字（i18n）。
  - 头部 spinner：WSL 请求 in-flight 时显示。
  - `SessionViewerModal` 传入会话的 `wslDistro` 供正文读取。
- i18n：`projectList.menu.wslSessions`、「不启用」、「（未找到）」、WSL 标识等，
  中英双语补齐（遵循 `src/i18n/locales/` 现有结构）。

### 平台

- 所有 WSL 逻辑仅 Windows 生效；macOS/Linux 上 `list_wsl_distros` 返回空、
  右键菜单项不显示（枚举为空即可自然隐藏）、`get_wsl_ai_sessions` 返回空。

## 验收标准

1. WSL 根项目（`\\wsl$\Ubuntu\home\u\proj`）打开后，Session 块自动列出该项目在
   WSL 内的 claude/codex 会话，无需任何配置。
2. Windows 路径项目右键 →「WSL 会话」→ 选择发行版后，Session 块合并显示
   Windows + WSL 两侧会话，按时间混排，WSL 条目带标识；重开应用配置仍生效。
3. 「不启用」后恢复只显示 Windows 会话。
4. WSL 会话可通过右键「查看」在 SessionViewerModal 中正常渲染正文。
5. WSL 会话右键「复制 resume 命令」得到 `claude --resume <id>`（或 codex 等价）。
6. WSL 未安装 / 发行版被卸载 / UNC 读失败：列表静默只显示 Windows 会话，无报错弹窗。
7. Windows 会话显示不被 WSL 冷启动阻塞（先出），WSL 加载中头部有 spinner。
8. `cd src-tauri && cargo test` 通过；新增路径映射/编码逻辑有单元测试。
9. 前端 `npm run build`（tsc + vite）通过。

# Research: WSL 内 claude/codex 会话的定位与读取

## 现有代码事实（2026-07-02 探索）

### src-tauri/src/ai_sessions.rs（全部 Windows 宿主视角）

- `get_ai_sessions(project_path)`：合并 claude + codex 会话，timestamp 降序，
  `SESSION_CACHE`（`HashMap<String, CachedSessions>`）按 `normalize_path(path)` 做 key，TTL 2s。
- claude 定位：`encode_project_path`（所有非 ASCII 字母数字字符 → `-`）拼出
  `~/.claude/projects/<encoded>`；前缀匹配变体时用 `dir_matches_project` 读 jsonl 前 5 行
  的 `cwd` 字段精确校验。标题取第一条非 `<` 开头的 user message，截 100 字符。
- codex 定位：递归收集 `~/.codex/sessions/**/*.jsonl`，读前 5 行找 `session_meta`，
  `payload.cwd` 与项目路径精确匹配；标题优先 `~/.codex/session_index.jsonl` 的 thread_name。
- 扫描上限：claude 300 文件 / codex 500 文件 / 每源 80 条 / 总 120 条。
- `normalize_path` 是 **Windows 语义**（`/`→`\` + lowercase + 去尾 `\`）——
  对 Linux cwd（`/mnt/d/git/foo`）不可复用，会把 `/` 换成 `\` 导致永不匹配。
  需新增 unix 语义 normalize（保留 `/`、lowercase、去尾 `/`）。
- `get_ai_session_content(session_type, session_id, project_path)` 读会话正文；
  claude 按 `<project_dir>/<id>.jsonl` 定位，codex 全量遍历找 `session_meta.payload.id` 匹配。

### src-tauri/mt-core/src/wsl_path.rs

- `parse_unc(path) -> Option<WslPath { distro, unix_path }>`，支持
  `\\wsl$\`、`\\wsl.localhost\`、`\\?\UNC\wsl$\`、`\\?\UNC\wsl.localhost\` 四种形式，
  host 大小写不敏感，distro 保留大小写。纯字符串解析、无磁盘访问。**可直接复用**。

### src/components/SessionList.tsx

- 单请求 `invoke('get_ai_sessions', { projectPath })`，`allSessions` 一个 state，
  前端分页（PAGE_SIZE=20，滚动加载）。刷新按钮重新调 fetch。
- 徽标 `TYPE_BADGE`: claude='C'/codex='X'。右键菜单：查看（SessionViewerModal）+
  复制 resume 命令。
- `SessionViewerModal` 接 `session` + `projectPath`，内部调 `get_ai_session_content`。

### src/components/ProjectList.tsx 右键菜单

- 菜单项：重命名 / 打开文件夹 / 复制路径 / 关联 SSH / 环境变量 / 分组移动。
- `src/utils/contextMenu.ts` 的 `MenuItem` 支持 `submenu?: MenuEntry[]`（悬停展开）、
  `disabled`、`MenuHeader`。**子菜单方案无需扩展 contextMenu**。
  注意：没有 `checked` 字段，选中态用 label 拼 ✓ 前缀实现。

## WSL 机制事实

### 会话文件位置

- WSL 内 claude：`/home/<user>/.claude/projects/<encoded-unix-cwd>/*.jsonl`，
  编码规则与 Windows 相同（非字母数字 → `-`），如 `/mnt/d/git/foo` → `-mnt-d-git-foo`、
  `/home/u/proj` → `-home-u-proj`。
- WSL 内 codex：`/home/<user>/.codex/sessions/<Y>/<M>/<D>/*.jsonl` +
  `/home/<user>/.codex/session_index.jsonl`，格式与 Windows 版一致（同一软件）。
- 从 Windows 访问：`\\wsl$\<distro>\home\<user>\...`（9P/Plan9 文件协议）。

### 发行版枚举（不 spawn 进程）

注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`：
- 每个子键（guid）下 `DistributionName`（REG_SZ）= 发行版名；
- 根键 `DefaultDistribution`（REG_SZ）= 默认发行版的 guid。
- 另有 `State`（REG_DWORD，1=installed）可过滤未安装完成的项；`Version`（1/2）。
- 避免 `wsl.exe -l -q`：spawn 开销 + stdout 是 UTF-16LE 编码坑。

### 行为注意点

- 读 `\\wsl$\<distro>\...` 时若该发行版 VM 未运行，**Windows 会自动启动它**
  （数秒延迟 + vmmem 常驻内存）。决议：接受（勾选即意味着用户在用 WSL）。
- 9P 逐文件读慢（每文件毫秒级往返），现有"扫 300 个文件读首行"模式在 WSL 侧要下调
  上限（决议：claude 100 / codex 200）。
- drvfs（/mnt/*）默认大小写不敏感（case=off），Windows 盘符挂载点为小写
  （`D:` → `/mnt/d`），盘符后的路径段保留原大小写——因此 cwd 匹配用 lowercase 比较安全。
- automount root 可被 `/etc/wsl.conf` 改掉（默认 `/mnt/`）。决议：不支持自定义 root。
- 多用户 distro：claude 可能装在任意用户 home。决议：枚举 `\home\*` + `\root`，
  凡含 `.claude`/`.codex` 的都扫，cwd 校验防串。

## 依赖

- 注册表读取：查 `src-tauri/Cargo.toml` 是否已有 `winreg`/`windows-registry`/`windows` 
  crate 可用；若无，加最小依赖（Windows-only，`[target.'cfg(windows)'.dependencies]`）。

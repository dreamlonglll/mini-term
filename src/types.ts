// === 配置持久化 ===

export type ProjectTreeItem = string | ProjectGroup;

export interface ProjectGroup {
  id: string;
  name: string;
  collapsed: boolean;
  children: ProjectTreeItem[];
}

export interface AppConfig {
  projects: ProjectConfig[];
  projectTree?: ProjectTreeItem[];
  // 旧字段仅用于迁移兼容（Rust 端处理后不再出现）
  projectGroups?: { id: string; name: string; collapsed: boolean; projectIds: string[] }[];
  projectOrdering?: string[];
  defaultShell: string;
  availableShells: ShellConfig[];
  uiFontSize: number;
  terminalFontSize: number;
  uiFontFamily?: string;
  terminalFontFamily?: string;
  terminalLigatures?: boolean;
  /** 每个终端保留的回滚行数。renderer 内存的大头,见 terminalCache 的 resolveScrollback */
  terminalScrollback?: number;
  layoutSizes?: number[];
  middleColumnSizes?: number[];
  theme: 'auto' | 'light' | 'dark';
  skin: 'none' | 'blueprint' | 'fluent2';
  terminalFollowTheme: boolean;
  aiCompletionPopup: boolean;
  aiCompletionTaskbarFlash: boolean;
  aiCompletionSound: boolean;
  aiCompletionSoundPath?: string;
  /** AI 转入「待确认」（权限审批 / MCP 表单 / 回合因 API 错误结束）时也走一遍
   *  上面三个通道提醒。与完成通知共用开关与自定义提示音，这里只管开不开 */
  aiAttentionNotify: boolean;
  editors: EditorConfig[];
  defaultEditor?: string;
  gitChangesViewMode: 'list' | 'tree';
  longPasteToFile: boolean;
  longPasteLineThreshold: number;
  longPasteCharThreshold: number;
  /** 远程项目粘贴落盘目录（剪贴板图片 / 长文本经 SFTP 上传的目标）。
   *  相对路径 = 相对项目根（默认 `.mini-term/pasted`）；也可填远端绝对路径或 `~/xxx` */
  remotePasteDir: string;
  /** 中间栏（Projects + Files）整体折叠开关 */
  middleColumnVisible: boolean;
  /** 右侧悬浮抽屉（Sessions / Git）宽度 */
  rightDrawerWidth?: number;
  lastActiveProjectId?: string;
  hookEnabled: boolean;
  smartCopyPaste: boolean;
  /** 状态栏(托盘/菜单栏)项目状态灯总开关;undefined = 开启 */
  trayStatusEnabled?: boolean;
  /** 托盘右键菜单最多显示的活跃项目数;undefined = 5 */
  trayMaxProjects?: number;
  /** 左键点状态栏图标时除唤起窗口外还定位到「下一个该处理」的会话;undefined = 开启 */
  trayClickFocus?: boolean;
  /** 启动恢复布局后自动续接上次的 AI 会话(写 resume 命令);undefined = 开启 */
  aiAutoResume?: boolean;
  /** 拖选按住不动自动复制的静止时长(秒);undefined = 1 */
  selectionAutoCopySecs?: number;
  sshConnections: SshConnection[];
  /** 显式创建的 SSH 分组名（允许空分组）。连接的 group 字段仍是归属单一来源 */
  sshGroups?: string[];
  /** 移动端中转配置(docs/adr/0001),未配置时缺省 */
  mobileRelay?: MobileRelayConfig;
  /** 激活的外置主题包 id（themes/ 下目录名）。undefined = 内置外观模式 */
  customThemeId?: string;
  /** AI 历史面板的会话列表视图;undefined = 平铺 */
  sessionListView?: 'flat' | 'tree';
  /** 会话分支自记账边(mini-term 自己发起的 fork)。磁盘扫描权威,这里兜
   *  「会话文件尚未落盘的窗口期」;合并按 child id 去重、磁盘优先 */
  sessionLineage?: LineageEdge[];
}

/** 会话分支边:sessionId fork 自 parentSessionId。与后端
 *  ai_sessions::LineageEdge / config::SavedLineageEdge 同构。 */
export interface LineageEdge {
  agent: string;
  sessionId: string;
  parentSessionId: string;
  /** 分叉点在父会话中的消息 uuid,仅 Claude 有此精度 */
  forkPointUuid?: string;
  /** 分支自己的首条用户消息(分叉后第一问)。fork 整份复制会让标题字段继承
   *  根会话,分支之间全同名——展示时优先用它;undefined 回落会话标题 */
  branchTitle?: string;
}

/** 移动端中转体系的持久化配置。字段对齐后端 #[serde(rename_all = "camelCase")]. */
export interface MobileRelayConfig {
  /** 中转服务器地址(如 wss://relay.example.com),空字符串 = 未配置、不建连 */
  relayUrl: string;
  /** 桌面端接入密钥,须与中转的 MT_RELAY_DESKTOP_KEY 一致;空 = 未填,连不上 */
  desktopKey?: string;
  /** AI 启动器列表:决定手机能起哪些 agent;命令与 shell 只存在于这里 */
  launchers?: AiLauncher[];
}

/** 一条具名的「怎么起一个 AI 会话」。 */
export interface AiLauncher {
  id: string;
  name: string;
  /** 引用 availableShells 里的条目名;缺省 = 用 defaultShell */
  shell?: string;
  command: string;
}

/** mobile-relay-status 事件 / mobile_relay_status 命令的载荷。 */
export interface MobileRelayStatusPayload {
  status:
    | 'disconnected'
    | 'connecting'
    | 'connected'
    | 'reconnecting'
    | 'versionMismatch'
    /** 密钥不匹配 */
    | 'authFailed'
    /** 中转未配置 MT_RELAY_DESKTOP_KEY(fail-closed) */
    | 'keyNotConfigured';
  /** versionMismatch 时携带,用于给出明确升级提示 */
  expectedVersion?: number;
  actualVersion?: number;
  /** 移动端配对状态(中转推送);undefined = 尚未知悉(未连上中转) */
  paired?: boolean;
}

/** mobile-rename-pane 事件载荷:移动端改会话名(标题已由后端收敛:去空白/控制字符/限长)。 */
export interface MobileRenamePanePayload {
  paneId: string;
  /** 空串 = 清除自定义名,回落 shell 名 */
  title: string;
}

/** mobile-start-session 事件载荷:移动端发起的一次会话创建请求。 */
export interface MobileStartSessionPayload {
  requestId: string;
  projectId: string;
  launcherId: string;
  /** 启动器展示名(通知文案用) */
  launcherName: string;
  /** 绑定的 shell 名;缺省 = 用默认 shell */
  shellName?: string;
  /** 要写入 PTY 的启动命令 */
  command: string;
}

/** 发起会话失败原因,对齐后端 StartSessionFailReason 的 camelCase 串。 */
export type StartSessionFailReason =
  | 'desktopOffline'
  | 'projectNotFound'
  | 'launcherNotFound'
  | 'notSupported'
  | 'spawnFailed';

/** `enable_ssh_tools` 返回值；projectToken 必须随项目配置持久化。 */
export interface EnableSshToolsResult {
  message: string;
  projectToken: string;
}

export interface ProjectConfig {
  id: string;
  name: string;
  path: string;
  /** 需求描述,显示在项目名后的灰色小字;undefined/空 = 不显示 */
  description?: string;
  savedLayout?: SavedProjectLayout;
  expandedDirs?: string[];
  /** 是否已为该项目启用 SSH 工具（向项目目录生成了 Claude / Codex 的 SKILL.md；
   *  字段名保留 Mcp 是为兼容存量配置，语义已是「SSH 工具（CLI + Skill）」） */
  sshMcpEnabled?: boolean;
  /** CLI/daemon 项目能力令牌；旧项目缺失时在下次保存「关联 SSH」时自动迁移。 */
  sshCliToken?: string;
  /** 该项目的 agent 可访问的 SSH 连接 id 列表（「关联 SSH」设定的范围）；undefined = 旧配置兼容,视为全部 */
  sshConnectionIds?: string[];
  /** 项目级环境变量,新建终端时注入到 PTY 子进程。已开终端不受影响。 */
  envVars?: ProjectEnvVar[];
  /** WSL 会话来源发行版名（「WSL 关联项目」声明）；undefined = 未启用。
   *  WSL 根项目（UNC 路径）不落此配置,distro 从路径自动推导。 */
  wslSessionsDistro?: string;
  /** SSH 远程项目：有值 = 该项目指向远程机器上的目录（引用 sshConnections 里的连接 id）。
   *  此时 `path` 存远程 POSIX 绝对路径。连接被删除 → 项目进入「断链」错误态。 */
  sshConnectionId?: string;
  /** 子项目(worktree「设为项目」)：有值 = 渲染在该父项目下方缩进一级,
   *  且**不进 projectTree**(树里只有顶层项目与分组)。拖出/「脱离父项目」时清除并入树。 */
  parentProjectId?: string;
  /** 项目类型徽标覆盖:undefined = 自动探测,'none' = 不显示,其余为技术栈 key。 */
  kindOverride?: ProjectKind | 'none';
}

/** 技术栈类型 key（项目类型徽标/探测结果）。展示名与探测规则在 utils/projectKind.ts。 */
export type ProjectKind =
  | 'java'
  | 'rust'
  | 'go'
  | 'python'
  | 'flutter'
  | 'php'
  | 'vuejs'
  | 'nextjs'
  | 'react'
  | 'svelte'
  | 'vite'
  | 'nodejs';

/** AI 厂商 key（pane 徽标/品牌图标）。推断规则在 utils/inferVendor.ts。 */
export type AiVendor =
  | 'claude'
  | 'openai'
  | 'pi'
  | 'gemini'
  | 'opencode'
  | 'grok'
  | 'qwen'
  | 'deepseek'
  | 'zhipu'
  | 'copilot'
  | 'ollama';

export interface ProjectEnvVar {
  key: string;
  value: string;
  /** 取消勾选时 value 保留但不注入,允许临时禁用某变量而无需删行重输 */
  enabled: boolean;
}

export interface ShellConfig {
  name: string;
  command: string;
  args?: string[];
}

export interface EditorConfig {
  name: string;
  command: string;
}

export interface SshConnection {
  id: string;
  name: string;
  host: string;
  port: number;
  user: string;
  password?: string;
  identityFile?: string;
  group?: string;
}

// === 布局持久化 ===

export interface SavedPane {
  shellName: string;
  /** 工作目录覆盖(worktree 终端):有值则替代项目根作为 PTY cwd */
  cwd?: string;
  /** 退出时该 pane 正在跑的 AI 会话;重启后据此自动 resume 续接 */
  aiSession?: AiSessionRef;
}

/** hook 上报的 AI 会话身份(agent 缺省按 Claude 处理)。 */
export interface AiSessionRef {
  agent?: string;
  sessionId: string;
  /** 会话启动目录:claude --resume 只认该目录对应的会话桶,续接时 PTY 以它为 cwd */
  cwd?: string;
}

export type SavedSplitNode =
  | { type: 'leaf'; panes: SavedPane[] }
  | { type: 'split'; direction: 'horizontal' | 'vertical'; children: SavedSplitNode[]; sizes: number[] };

export interface SavedTab {
  customTitle?: string;
  splitLayout: SavedSplitNode;
}

/**
 * 磁盘上的项目布局。
 *
 * `tabs` 是历史包袱:曾经有一层「项目级 tab」，但界面上从来没有切换入口，
 * 那层运行时状态已删除（终端标签的唯一出口是 PaneGroup 的 tab 栏）。
 * 磁盘格式保留原样是为了向后兼容 Rust 端 `SavedProjectLayout` 与旧 config.json —
 * **写出时恒为单元素**；读取旧配置遇到多元素时，后续 tab 的 pane 会被合并进
 * 第一棵布局树（见 layoutRestore.ts），不丢用户的终端。
 */
export interface SavedProjectLayout {
  tabs: SavedTab[];
  activeTabIndex: number;
}

// === 运行时状态 ===

export type PaneStatus = 'idle' | 'ai-idle' | 'ai-working' | 'error';

export interface ProjectState {
  id: string;
  /** 该项目的终端布局树；null = 还没有终端（渲染空态） */
  layout: SplitNode | null;
  /** 由 layout 聚合出的项目级状态（error > ai-working > ai-idle > idle） */
  status: PaneStatus;
  needsAttention?: boolean;
}

export interface AiCompletionNotification {
  id: string;
  projectId: string;
  projectName: string;
  timestamp: number;
  /** 通知类型,默认 'ai-completion'(AI 任务完成,点击跳到对应项目);
   *  'ai-attention' 用于 AI 转入待确认(警告色,点击跳到对应项目);
   *  'wsl-info' 用于 WSL 启动器重写提示,不携带 projectId 跳转语义;
   *  'mobile-session' 用于移动端远程发起的新会话(点击跳到对应项目);
   *  'paste-error' 用于远程粘贴上传失败(错误态图标,点击仅关闭)。 */
  kind?: 'ai-completion' | 'ai-attention' | 'wsl-info' | 'mobile-session' | 'paste-error';
  /** kind='wsl-info' / 'mobile-session' 时的自定义消息文本,渲染时直接展示。 */
  message?: string;
}

export type SplitNode =
  | { type: 'leaf'; panes: PaneState[]; activePaneId: string }
  | { type: 'split'; direction: 'horizontal' | 'vertical'; children: SplitNode[]; sizes: number[] };

export interface PaneState {
  id: string;
  shellName: string;
  customTitle?: string;
  status: PaneStatus;
  ptyId?: number;
  /** 工作目录覆盖(worktree 终端):有值则替代项目根作为 PTY cwd,随布局持久化 */
  cwd?: string;
  /** 当前/上次 AI 会话身份(hook 上报),随布局持久化;会话正常退出时清除。
   *  身份在 resume 后**保留**(codex resume 不会重新上报 SessionStart,
   *  写完即清会让身份在第二次重启时断代),hook 上报新身份时自然覆盖。 */
  aiSession?: AiSessionRef;
  /** 待续接标记:恢复布局时随 aiSession 置位,PaneGroup 起 PTY 写完 resume
   *  命令后清除(只清标记不清身份);运行时状态不持久化。 */
  resumePending?: boolean;
  /** 后端识别的会话内 AI 命令名(输入检测/hook 兜底);运行时状态不持久化。
   *  品牌图标优先用 aiSession.agent,无 hook 时靠它。 */
  detectedAgent?: string;
  /** ai-idle 的成因是「需要用户确认」(授权/输入请求);运行时状态不持久化 */
  attention?: boolean;
}

// === pane 预览缩略图(panePreview.ts 提取 → panePreviewCanvas.ts 绘制) ===

/** 同色连续字符段;col 为起始列,绘制定位 x = col × cellW */
export interface PreviewRun {
  col: number;
  text: string;
  color: string;
}

export interface PreviewGrid {
  cols: number;
  rows: number;
  /** 每视口行的 runs;空白不产生 run */
  lines: PreviewRun[][];
}

export interface PreviewPaletteOptions {
  /** ANSI 16 色(black..white, brightBlack..brightWhite),来自终端主题 */
  palette16: string[];
  /** 默认前景色(theme.foreground) */
  foreground: string;
}

/** `get_ai_hook_registrations` 返回的单条注册现状。对齐后端 HookRegistrationInfo。 */
export interface HookRegistration {
  /** 注册目标 key,回传给 register/unregister_ai_hooks 的 agents 参数 */
  agent: 'claude' | 'codex' | 'grok';
  /** 展示名(Claude Code / Codex / Grok) */
  label: string;
  /** 配置文件路径(~ 缩写) */
  file: string;
  /** 该文件里属于 mini-term 的事件条目数;0 = 没注册过 */
  registered: number;
  /** 当前版本应注册的事件总数;0 < registered < total = 旧事件集,需重新注册补齐 */
  total: number;
}

// === AI 会话 ===

export interface AiSession {
  id: string;
  sessionType: 'claude' | 'codex' | 'grok';
  title: string;
  timestamp: string; // ISO 8601
  /** 会话最新使用的模型(后端尾窗反扫)。CLI ≠ 模型厂商——分支 UI 按它推
   *  厂商图标(vendorForSession);undefined = 识别不出,回落 CLI 图标 */
  model?: string;
  /** 会话来源:有值 = 该 WSL 发行版内的会话,undefined = Windows 宿主会话 */
  wslDistro?: string;
  /** 会话来源:有值 = 该 SSH 连接指向的远程机器上的会话（与 wslDistro 互斥） */
  sshConnectionId?: string;
}

/** list_wsl_distros 返回的单条发行版记录 */
export interface WslDistro {
  name: string;
  isDefault: boolean;
}

export interface AiSessionMessage {
  role: 'user' | 'assistant';
  content: string;
  timestamp: string;
}

/** ssh_remote_ai_session_content 返回值（对齐 Rust RemoteSessionContent camelCase 序列化） */
export interface RemoteSessionContent {
  /** 本次解析出的消息（与本地 get_ai_session_content 的元素同构） */
  messages: AiSessionMessage[];
  /** 下次增量读取应传入的字节偏移。首次调用传 offset=0（或省略）拿全量 */
  nextOffset: number;
}

/** create_pty 的可选远程启动参数（对齐 Rust SshRemoteSpec camelCase 反序列化） */
export interface SshRemoteSpec {
  connectionId: string;
  remotePath: string;
}

// === 文件树 ===

export interface FileEntry {
  name: string;
  path: string;
  isDir: boolean;
  ignored?: boolean;
  children?: FileEntry[];
  /** 单链目录汇总(compact)后链上各段的真实路径(含链首与链尾)。前端
   *  compactDirChains 附加,非后端字段;watch 注册与中段变化判定用。 */
  chainPaths?: string[];
}

// === Tauri 事件 payload ===

export interface PtyOutputPayload {
  ptyId: number;
  data: string;
}

export interface PtyExitPayload {
  ptyId: number;
  exitCode: number;
}

export interface PtyStatusChangePayload {
  ptyId: number;
  status: PaneStatus;
  /**
   * 状态变化的成因：hook 直推时是（归一化后的）hook 事件名（`Stop` /
   * `PermissionRequest` / `SessionEnd` …），后端 monitor 轮询算出的变化没有该字段。
   *
   * 多个 hook 事件都落到 `ai-idle`，但只有 `Stop` 表示"任务做完了"——权限请求、
   * 通知、澄清同样是 ai-idle，播报成完成就是误报（见 `isAiCompletion`）。
   * 托盘黄灯认 `PermissionRequest`/`Elicitation`（权限/确认类 Notification
   * 已在后端按文案归一化为 `PermissionRequest`）。
   */
  cause?: string;
  /** 会话内 AI 命令名(claude/codex/opencode…),品牌图标兜底用;缺省 = 未知 */
  agent?: string;
}

/** load_config 命令返回:配置 + 本次写盘令牌(config.rs LoadedConfig 镜像)。 */
export interface LoadedConfig {
  config: AppConfig;
  token: number;
}

/** pty-ai-session 事件载荷:hook 上报的 AI 会话身份,供重启后 resume 续接。 */
export interface PtyAiSessionPayload {
  ptyId: number;
  agent?: string;
  sessionId: string;
  cwd?: string;
}

export interface FsChangePayload {
  projectPath: string;
  path: string;
  kind: string;
}

// === 搜索 ===

export interface SearchResultItem {
  filePath: string;
  fileName: string;
  lineNumber?: number;
  lineContent?: string;
  matchRanges: [number, number][];
}

export interface SearchResultsPayload {
  searchId: string;
  items: SearchResultItem[];
}

export interface SearchCompletePayload {
  searchId: string;
  totalCount: number;
  cancelled: boolean;
}

// === Git 状态 ===

export type GitStatusType = 'modified' | 'added' | 'deleted' | 'renamed' | 'untracked' | 'conflicted';

export interface GitFileStatus {
  path: string;
  oldPath?: string;
  status: GitStatusType;
  statusLabel: string; // "M", "A", "D", "R", "?", "C"
}

export interface ChangeFileStatus {
  path: string;
  oldPath?: string;
  stagedStatus?: GitStatusType;
  unstagedStatus?: GitStatusType;
  statusLabel: string;
}

export interface DiffHunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  lines: DiffLine[];
}

export interface DiffLine {
  kind: 'add' | 'delete' | 'context';
  content: string;
  oldLineno?: number;
  newLineno?: number;
}

export interface GitDiffResult {
  oldContent: string;
  newContent: string;
  hunks: DiffHunk[];
  isBinary: boolean;
  tooLarge: boolean;
}

// === 文件查看 ===

export interface FileContentResult {
  content: string;
  isBinary: boolean;
  tooLarge: boolean;
}

// === Git 历史 ===

export interface GitRepoInfo {
  name: string;
  path: string;
  currentBranch?: string;
  /** 该条目是不是某个主仓库的 linked worktree */
  isWorktree?: boolean;
}

/** list_worktrees 返回的单条工作区记录(主工作区 + linked worktree) */
export interface WorktreeInfo {
  name: string;
  path: string;
  /** HEAD 所在分支;detached / 失效条目为 undefined */
  branch?: string;
  isMain: boolean;
  /** false = 目录已丢失/元数据损坏,可 prune 的失效条目 */
  isValid: boolean;
  isLocked: boolean;
}

export interface GitCommitInfo {
  hash: string;
  shortHash: string;
  message: string;
  body?: string;
  author: string;
  timestamp: number;
  /** 全部父提交 hash（第 0 个是主线父），用于绘制分支拓扑图 */
  parentHashes: string[];
}

export interface CommitFileInfo {
  path: string;
  status: 'added' | 'modified' | 'deleted' | 'renamed';
  oldPath?: string;
}

export interface BranchInfo {
  name: string;
  isHead: boolean;
  isRemote: boolean;
  commitHash: string;
}

// === AI 任务分段 marker ===

export interface AiUserSubmitPayload {
  ptyId: number;
  line: string;
  ts: number;
}

export interface AiMarker {
  id: string;            // UUID,store 索引与 React key
  seq: number;           // 该 pane 内自增序号,UI 显示 "#N"
  ptyId: number;
  line: string;          // 用户输入原文(trim 后)
  ts: number;            // epoch ms
  xtermMarkerId: number; // xterm IMarker.id,用于查找 module-local 缓存
  inProgress: boolean;   // 最后一个 marker 为 true,新 marker 到来时前一个翻 false
}

// === 使用统计（对齐 Rust usage_stats camelCase 序列化） ===

export type UsageAgentFilter = 'all' | 'claude' | 'codex' | 'grok';
export type UsageRange = 'today' | 'days7' | 'days30' | 'month' | 'months3' | 'months6' | 'custom';

/** 单模型价格（$/token，前端拉 models.dev 后 ÷1e6 归一） */
export interface ModelPriceEntry {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

export interface UsageDailyStat {
  /** 日粒度 "YYYY-MM-DD"；「今天」视图为小时粒度 "HH:00"（均本地时区） */
  date: string;
  cost: number;
  calls: number;
  /** hover 详情用的 token 明细 */
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
}

export interface UsageProjectStat {
  path: string;
  name: string;
  cost: number;
  sessions: number;
  calls: number;
  tokens: number;
}

export interface UsageTopSessionStat {
  sessionId: string;
  agent: string;
  projectPath: string;
  projectName: string;
  title: string;
  timestamp: string; // "YYYY-MM-DD"（本地日历日）
  cost: number;
  calls: number;
  tokens: number;
}

export interface UsageModelStat {
  /** 归一后的模型名（剥日期/provider 前缀）；空串 = 未知模型 */
  model: string;
  cost: number;
  calls: number;
  tokens: number;
}

export interface UsageProviderStat {
  /** 供应商展示名（baseurl 的 host） */
  provider: string;
  cost: number;
  calls: number;
  tokens: number;
  sessions: number;
}

/** 计数排行条目（工具/Shell/MCP，设计 §2.2 各前 10） */
export interface UsageCountStat {
  name: string;
  count: number;
}

export interface UsageStatsPayload {
  totalCost: number;
  totalCalls: number;
  sessionCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  daily: UsageDailyStat[];
  byProject: UsageProjectStat[];
  byModel: UsageModelStat[];
  byProvider: UsageProviderStat[];
  topSessions: UsageTopSessionStat[];
  byTool: UsageCountStat[];
  byShell: UsageCountStat[];
  byMcp: UsageCountStat[];
}

export interface UsageLedgerProgressPayload {
  /** backfill（账本首建全量同步）进度：已处理/总文件数 */
  processed: number;
  total: number;
}

export interface UsageLedgerSyncedPayload {
  /** 本轮增量同步重解析的文件数；0 = 账本无变化 */
  added: number;
}

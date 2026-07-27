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
  layoutSizes?: number[];
  middleColumnSizes?: number[];
  theme: 'auto' | 'light' | 'dark';
  skin: 'none' | 'blueprint' | 'fluent2';
  terminalFollowTheme: boolean;
  aiCompletionPopup: boolean;
  aiCompletionTaskbarFlash: boolean;
  aiCompletionSound: boolean;
  aiCompletionSoundPath?: string;
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
  sshConnections: SshConnection[];
  /** 显式创建的 SSH 分组名（允许空分组）。连接的 group 字段仍是归属单一来源 */
  sshGroups?: string[];
  /** 移动端中转配置(docs/adr/0001),未配置时缺省 */
  mobileRelay?: MobileRelayConfig;
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

export interface ProjectConfig {
  id: string;
  name: string;
  path: string;
  savedLayout?: SavedProjectLayout;
  expandedDirs?: string[];
  /** 是否已为该项目启用 SSH MCP（向项目目录写入了 Claude / Codex 的 MCP 注册配置） */
  sshMcpEnabled?: boolean;
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
}

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
   *  'wsl-info' 用于 WSL 启动器重写提示,不携带 projectId 跳转语义;
   *  'mobile-session' 用于移动端远程发起的新会话(点击跳到对应项目);
   *  'paste-error' 用于远程粘贴上传失败(错误态图标,点击仅关闭)。 */
  kind?: 'ai-completion' | 'wsl-info' | 'mobile-session' | 'paste-error';
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
}

// === AI 会话 ===

export interface AiSession {
  id: string;
  sessionType: 'claude' | 'codex';
  title: string;
  timestamp: string; // ISO 8601
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

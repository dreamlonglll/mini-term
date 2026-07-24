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
  projectsVisible: boolean;
  sessionsVisible: boolean;
  filesVisible: boolean;
  gitVisible: boolean;
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
}

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
}

export type SavedSplitNode =
  | { type: 'leaf'; panes: SavedPane[] }
  | { type: 'split'; direction: 'horizontal' | 'vertical'; children: SavedSplitNode[]; sizes: number[] };

export interface SavedTab {
  customTitle?: string;
  splitLayout: SavedSplitNode;
}

export interface SavedProjectLayout {
  tabs: SavedTab[];
  activeTabIndex: number;
}

// === 运行时状态 ===

export type PaneStatus = 'idle' | 'ai-idle' | 'ai-working' | 'error';

export interface ProjectState {
  id: string;
  tabs: TerminalTab[];
  activeTabId: string;
  needsAttention?: boolean;
}

export interface AiCompletionNotification {
  id: string;
  projectId: string;
  projectName: string;
  timestamp: number;
  /** 通知类型,默认 'ai-completion'(AI 任务完成,点击跳到对应项目);
   *  'wsl-info' 用于 WSL 启动器重写提示,不携带 projectId 跳转语义。 */
  kind?: 'ai-completion' | 'wsl-info';
  /** kind='wsl-info' 时的自定义消息文本,渲染时直接展示。 */
  message?: string;
}

export interface TerminalTab {
  id: string;
  customTitle?: string;
  splitLayout: SplitNode;
  status: PaneStatus;
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
}

export interface GitCommitInfo {
  hash: string;
  shortHash: string;
  message: string;
  body?: string;
  author: string;
  timestamp: number;
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

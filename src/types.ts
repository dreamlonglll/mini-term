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
  lastActiveProjectId?: string;
  hookEnabled: boolean;
  smartCopyPaste: boolean;
  sshConnections: SshConnection[];
  /** cc-connect 集成配置(进程管理 + 项目导入 + dashboard 嵌入),未配置时缺省 */
  ccConnect?: CcConnectConfig;
}

export interface CcConnectConfig {
  /** cc-connect 可执行文件路径,空字符串 = 让前端 PATH 探测 */
  exePath: string;
  /** config.toml 路径,空字符串 = 默认 ~/.cc-connect/config.toml */
  configPath: string;
  /** mini-term 启动时自动 spawn cc-connect */
  autoStart: boolean;
  /** 额外启动参数 */
  extraArgs: string[];
  /** mini-term project id → cc-connect project name 映射 */
  projectLinks: Record<string, string>;
}

/** cc_connect_probe 返回值。字段命名对齐后端 #[serde(rename_all = "camelCase")]. */
export interface CcConnectStatus {
  running: boolean;
  port: number;
  version?: string;
  /** mini-term 自己 spawn 的 cc-connect PID,用户手动启动时为 undefined */
  ownPid?: number;
  /** 探活失败时的友好诊断信息(token 缺失 / 端口不通 / 配置文件不存在等) */
  diagnostic?: string;
}

/** cc_connect_list_projects 返回的单条项目记录。 */
export interface CcProject {
  name: string;
  workDir?: string;
  agentType?: string;
  hasPlatform: boolean;
}

/** cc_connect_import_project 的请求载荷。 */
export interface ImportProjectRequest {
  name: string;
  workDir: string;
  agentType?: string;
}

/**
 * cc_connect_import_project 返回值。
 *
 * 后端在 toml 已写盘但 cc-connect restart 失败时不再返 Err,而是把 restartOk=false 编码到 result 里,
 * 让前端按 tomlWritten && !restartOk 仍然写入 projectLinks(避免"项目存在但未关联"半同步态)。
 */
export interface ImportProjectResult {
  name: string;
  tomlWritten: boolean;
  restartOk: boolean;
  restartError?: string;
}

/**
 * cc_connect_unlink_project 返回值。语义与 ImportProjectResult 对称:
 * deletedOk=true 但 restartOk=false 时,前端仍清理本地 projectLinks。
 */
export interface UnlinkProjectResult {
  name: string;
  deletedOk: boolean;
  restartOk: boolean;
  restartError?: string;
}

/** cc_connect_import_projects(批量导入)返回值:一次写盘 + 仅重启一次。 */
export interface BatchImportResult {
  imported: string[];
  skipped: string[];
  tomlWritten: boolean;
  restartOk: boolean;
  restartError?: string;
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

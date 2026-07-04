<p align="center">
  <img src="src-tauri/icons/icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>A desktop terminal manager built for the AI era</strong><br>
  Powered by Tauri v2 · Multi-project · Multi-tab · Split-pane layout · AI process awareness · Remote driving over IM platforms (cc-connect)
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.6.5-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/Tauri-v2-orange" alt="tauri">
  <img src="https://img.shields.io/badge/React-19-61dafb" alt="react">
  <img src="https://img.shields.io/badge/Rust-2021-dea584" alt="rust">
</p>

---

## Why Mini-Term

1. **Heavyweight tools are overkill** — All-in-on-AI users only need a terminal to run their agents, yet are forced to fire up heavy IDEs like VS Code / IDEA that are large and memory-hungry.
2. **No awareness of concurrent agents** — When several Claude / Codex sessions run at once, there's no clear way to see which agent has finished.
3. **Project switching is clumsy** — The system terminal lacks multi-project organization, tabs, and split-pane management.

Mini-Term solves all of the above with one lightweight desktop app.

## Preview

![Main UI](docs/screenshots/main.png)
![Settings UI](docs/screenshots/settings.png)


## Features

### Terminal Core

- **Multi-tab management** — A dedicated tab per project, drag to reorder, status icons at a glance.
- **Recursive splitting** — Arbitrarily nested horizontal / vertical splits, drag to adjust ratios via Allotment.
- **High-performance rendering** — xterm.js v6 + WebGL acceleration, automatic fallback to Canvas.
- **100k-line scrollback buffer** — Intercepts the CSI 3J (ED3) sequence so scrollback survives when TUIs like Claude / Codex / OpenCode clear the screen; intercepts alternate-screen switching so TUI output stays in the main buffer, keeping the scrollbar and scrollback always available.
- **Terminal caching** — Switching projects / tabs / panes never rebuilds the xterm instance, so existing content is preserved; lazy startup creates a PTY only for the currently visible pane, avoiding the slowdown of spawning more terminals the more history projects you have.
- **Project-switch caching** — FileTree / GitHistory data is cached per project, so switching back to a visited project renders with zero latency; directory loading and Git status run in parallel, and Git repo scan results are cached for 30 seconds.
- **Copy & paste** — `Ctrl+Shift+C/V` (macOS `⌘+Shift+C/V`) shortcuts + context menu, with "Copy" auto-greyed when nothing is selected; an optional "Smart `Ctrl+C/V`" mode (copy when there's a selection, interrupt the program when there isn't, and `Ctrl+V` pastes directly); on Windows, large multi-line pastes are chunked to prevent ConPTY from dropping lines.
- **Long-text paste** — When clipboard text is ≥10 lines or ≥2000 chars, it is automatically saved to a temporary `.txt` and a quoted file path is pasted instead, avoiding the performance and paste-bracket issues of feeding huge content straight to AI tools.
- **Image paste** — Detects screenshots in the clipboard, saves them to a temporary PNG via the Win32 API, and pastes a quoted path; compatible with non-standard formats such as PinPix.
- **File drag & drop** — Dragging a file from the file tree or system file explorer onto the terminal inserts its quoted absolute path, targeting the exact split pane and handling paths with spaces.
- **Multiple shell profiles** — Windows (cmd / powershell / pwsh), macOS (zsh / bash), Linux (bash / sh) and more, freely added or removed.

### SSH Connections

- **Connection management** — The top-bar "SSH" button opens a management dialog to add / edit / delete SSH connections, with host / port / username / password / private key / group fields, persisted to the config file.
- **Quick connect** — A right-click "SSH Connect" submenu inside the terminal lists saved connections by group; selecting one assembles the `ssh` command and launches the session right in the current terminal.
- **Password auto-fill** — For connections with a saved password, the backend scans PTY output for the password prompt and writes the password back automatically, once per session, stopping on a wrong password to avoid hammering the server with bad credentials.
- **Private-key permission handling** — When connecting with a private key, the key is copied to a permission-tightened temporary copy (Windows `icacls` / Unix `0600`) to bypass OpenSSH's "UNPROTECTED PRIVATE KEY FILE" rejection, without modifying your original key file.
- **Advanced capabilities** — Key-file login (`ssh -i`) and connection grouping.
- **SSH MCP Server** — Exposes saved SSH connections as MCP tools to AI agents running in the terminal (Claude Code / Codex). The project right-click "Link SSH" menu enables them per project and limits visibility to the selected connections; the built-in `mt-ssh-mcp` sidecar (a stdio MCP server based on the official rmcp) provides `ssh_list_connections`, `ssh_exec`, `ssh_upload` and `ssh_download`, where `ssh_exec` reuses password / private-key auth with timeouts, output capping, and an audit log; `ssh_upload` / `ssh_download` transfer single files over SFTP (streamed in chunks for constant memory, so large files work, unlike base64-echoing through `ssh_exec`), download writes straight to a local path on disk, and a hard guard refuses to ever transfer mini-term's own `config.json` (which holds all saved SSH credentials); enabling / disabling writes idempotently into Claude's `.mcp.json` and Codex's `.codex/config.toml` using named markers. **Since v0.4.10 the sidecar maintains an in-process SSH session pool** (russh 0.61 + tokio): the first call to a connection does one TCP handshake + auth (~seconds), and subsequent commands cost only an RTT; sessions are recycled after 10 minutes idle or 2 hours max, and gracefully `disconnect` when the sidecar exits.

### WSL Support (Windows)

- **WSL directories as project roots** — Supports adding WSL paths in both `\\wsl$\<distro>\<unix-path>` and `\\wsl.localhost\<distro>\<unix-path>` forms as projects; the displayed path automatically strips the `\\?\UNC\` verbatim prefix, and the file tree expands and previews normally.
- **Automatic wsl.exe launch** — When the cwd is detected as a WSL UNC path, `create_pty` ignores the user-configured shell (cmd / pwsh, etc.) and forces `wsl.exe -d <distro> --cd <unix-path>`, so the cwd truly lands inside WSL (`pwd` shows `/home/<user>/proj` rather than `C:\Windows`), consistent with Windows Terminal's `MangleStartingDirectoryForWSL` behavior; the distro name is parsed directly from the path without invoking `wsl -l -v`, and a one-time toast appears when the rewrite triggers.
- **Known limitations** — AI process detection (ai-working / ai-idle states) relies on the host's `process_monitor` reading child process names; after a wsl.exe launch, the `claude` / `codex` processes inside the WSL VM are out of the monitor's scope, so AI status stops working. `notify` file watching very likely loses events on the WSL 9P filesystem, so the file tree needs a manual refresh. Verified only on WSL2; WSL1 compatibility is not guaranteed.

### File Search

- **Global search** — Triggered by `Ctrl+Shift+F` (macOS `⌘+Shift+F`) or the file-tree toolbar button, supporting both filename and file-content search modes.
- **Regex matching** — Toggle between substring / regex modes, with matched keywords highlighted in the results.
- **Streaming results** — The backend walks the file tree with the `ignore` crate and pushes results in batches every 50 entries or 100ms, cancellable at any time.
- **Content grouping** — Content-search mode groups matched line numbers by file; clicking a result previews and jumps straight to the matched line.

### AI Process Awareness

- **Hook event system** — Integrates the official Claude Code / Codex Hook APIs to receive AI tool events (SessionStart / End, ToolUse, etc.), which is more precise and timely than process polling; the built-in `miniterm-hook` CLI is called by the hook system to POST events to a local server; the settings UI registers / unregisters the hook config with one click, merging rather than overwriting your existing hooks.
- **Real-time status detection** — Hook-first with a 500ms process-polling fallback, auto-detecting Claude / Codex / OpenCode and showing idle / working / error states.
- **Status aggregation** — Aggregated layer by layer from pane → tab → project, with priority `error > ai-working > ai-idle > idle`.
- **Completion notification trio** — Fires the moment an AI task goes working → idle:
  - A bottom-right toast desktop notification (only for inactive projects, deduplicated per project).
  - A DONE badge in the project list, cleared on click.
  - Taskbar flashing (Windows) / Dock bouncing (macOS), triggered only when the window is unfocused.
  - A notification sound (a default tone synthesized via the Web Audio API, with support for a custom audio file).
  - All notification toggles are independently configurable, managed on a dedicated "AI Completion Notifications" page in the settings center.
- **Session enter/exit detection** — Recognizes entering AI via command echo; recognizes exit via a double `Ctrl+C` / `Ctrl+D` or `exit` / `quit` / `:quit` / `/logout`.
- **Session history** — Reads local Claude / Codex history records, with a right-click to copy the resume command for quick continuation; the first screen renders only 20 entries, loading more automatically as you scroll to the bottom.
- **Session viewer** — A right-click "View" shows the full conversation, with User as plain text and Assistant rendered as Markdown (external links open in the system default browser after a confirmation prompt), supporting `Ctrl+F` search highlighting and quick navigation between User messages.
- **WSL sessions** — Reads Claude / Codex session history inside WSL distros directly from Windows (no `wsl.exe` spawn — via `\\wsl$` UNC plus registry-based distro enumeration): WSL-rooted projects auto-derive the distro and path with zero configuration; Windows-path projects pick a distro via the right-click "WSL Sessions" submenu and are scanned through `/mnt` path mapping, with in-session cwd verification to prevent cross-project mixing; WSL sessions merge chronologically with local ones under a WSL badge, a header spinner shows while loading, and viewing session content is supported too.
- **AI task markers** — Each time the user presses Enter inside an AI session, a marker is dropped in xterm; the ⚑ button at the tab's top-right drops down the list of past submissions, and clicking one or pressing `Ctrl+Shift+↑/↓` (macOS `⌘+Shift+↑/↓`) jumps between markers, briefly highlighting the target line.

### cc-connect Integration

Remotely drive the AI agents running on your machine through IM platforms (Feishu / Slack / Telegram / Discord / DingTalk / WeChat, etc.), bridged with the upstream [chenhg5/cc-connect](https://github.com/chenhg5/cc-connect). All entry points are gathered in the top-bar "Connect" button dialog.

- **Process management (zero-config)** — Start / stop / restart / test-connection / edit config.toml from within the "Connect" dialog, optionally auto-spawned on mini-term startup; when the executable / config paths are left blank, it falls back to `cc-connect` on PATH and `~/.cc-connect/config.toml`, working with zero configuration; on Windows it resolves the npm script shell (`.cmd` / `.ps1`) via PATH × PATHEXT, and stop / restart use `taskkill /T` to kill the whole process tree to avoid orphans; closing mini-term does not kill it, keeping IM available continuously.
- **One-click project import** — The "Connect" dialog lists all mini-term projects; check / select-all to batch-import in one click (appending `[[projects]]` via `toml_edit` to preserve comments, a single disk write + a single cc-connect restart, with an 8-char hash suffix auto-added on name conflicts), or import individually; imported entries show "● Imported" and can be removed in one click. Each imported project comes with a **placeholder Telegram platform** (cc-connect requires at least one platform per project or cold start fails); after importing, replace the placeholder with a real IM platform in the Dashboard to enable it.
- **Embedded Dashboard** — "Open Dashboard" in the "Connect" dialog opens cc-connect's own web console in an iframe (already authenticated), letting you configure IM platforms / switch providers / manage cron directly inside mini-term without opening a browser; it `createPortal`s to `document.body` to bypass the backdrop-filter of Fluent 2's `[data-panel]` and ensure full-screen coverage; keep-alive close uses `display:none` without unmounting, avoiding re-login.
- **Half-synced state handling** — Even if cc-connect's restart fails on project import / removal, the frontend keeps the local `projectLinks` based on the `tomlWritten` / `deletedOk` branches and shows a warning toast explaining the restart failure; the change takes effect the next time cc-connect starts.
- **Graceful degradation** — When cc-connect isn't running, buttons like Import / Dashboard are greyed out with a "start cc-connect first" hint rather than crashing; when config.toml has no `[management].token`, it gives a friendly diagnostic guiding the user to run `cc-connect web` to generate one automatically.

### Project Management

- **Project list** — Manage multiple project directories in the left sidebar, switch workspaces in one click, and restore the last active project on restart.
- **Drag to add projects** — Drag a folder from the file explorer onto the project list to add it quickly, with automatic detection of files / folders / duplicate projects and visual feedback.
- **Nested groups** — Up to 3 levels of project grouping, drag to reorder, collapse / expand, with a group context menu to add a project directly into that group.
- **File tree** — An integrated directory browser with natural sorting (V1 → V2 → V10 rather than lexicographic), nested `.gitignore` greying (ignore rules and `!pattern` allowlists at every sub-directory level take effect, consistent with git behavior), and live refresh via `notify` file watching.
- **File operations** — Create / rename / delete files and folders and view contents inside the file tree (Markdown rendering supports HTML tags and external images, external links open in the system default browser after a confirmation prompt, image formats are shown directly, HTML files preview in an iframe with relative-path resources auto-resolved, and binary / oversized files get a friendly notice).
- **Open in external editor** — A button at the top-right of the file tree opens the current project in your configured editor (VS Code by default), with the path customizable under "Settings → System → External Editor"; files can be opened with the system default app.
- **Project-level environment variables** — The project context menu "Environment Variables…" opens a management dialog with a row-level `[enable checkbox][key][value][✕]` layout, injecting per-project variables into the PTY child process when starting that project's terminal; strict POSIX validation (key matches `^[A-Za-z_][A-Za-z0-9_]*$`, no `MINITERM_` prefix, no `WSLENV`, no duplicates within a project, and value forbids `\n/\r/\0`); the Rust side adds a defensive `MINITERM_`-prefix + `WSLENV` filter, so even hand-editing `config.json` to bypass frontend validation cannot break the hook protocol or WSLENV concatenation; under WSL projects, variables pass through to Linux bash via the WSLENV mechanism (`/u` is one-way without path translation; an `export` of the same name in `~/.bashrc` will override).

### Git Integration

- **File status** — The file tree shows Git status colors (modified / added / deleted / conflict).
- **Change diff** — A detailed diff of working-tree file changes, parsed at the hunk/line level, with side-by-side / inline dual views; side-by-side mode supports dragging to adjust the split ratio, and the font size follows the terminal font setting.
- **Commit history** — Browse the repo's commit log with cursor-based pagination (30 entries by default).
- **Commit diff** — View the file changes of any commit, switching file by file.
- **Branch info** — Local / remote branch lists.
- **Source control panel** — A VS Code-style Changes panel grouping Staged / Changes / Untracked, supporting per-file and bulk stage / unstage / discard, `Ctrl+Enter` to commit quickly, and toggling between list and tree views.
- **Pull / Push** — In-row buttons sync with the remote in one click, with a refresh button to reload the commit log and branch info.
- **Multi-repo discovery** — Automatically scans all Git repos under the project directory (recursing 5 levels, skipping `node_modules` etc.).

![Git integration](docs/screenshots/git.png)

### Appearance & Configuration

- **Activity Bar sidebar** — A persistent 40px icon bar on the far left with Projects / Sessions / Files / Git panel toggles, independently controlling visibility, a blue vertical bar indicating the active state, and persisted state.
- **Three theme modes** — Auto (follows the system) / Light / Dark, with Dark based on a Warm Carbon palette and a custom CSS-variable system; the native Windows title bar (DWM Immersive Dark Mode) follows the theme automatically, with no first-frame light flash for dark-mode users on startup.
- **Blueprint skin** — An optional sci-fi Blueprint skin with a grid background + corner markers + glow effects, supporting both dark and light modes, with the terminal palette switching in sync.
- **Independent font tuning** — The UI and terminal font sizes (10-20px) / families are adjustable separately, and the terminal can optionally follow the UI theme.
- **Ligatures** — A terminal ligature toggle that composes glyphs like `==` `=>` `!=` `->` when enabled, requiring a font with a calt table (Fira Code / JetBrains Mono); fully supported on Windows, while macOS / Linux use a 60-entry Iosevka fallback due to webview API limitations.
- **Layout persistence** — Split ratios, tabs, and window size / position are saved automatically and restored on restart (`tauri-plugin-window-state`).
- **Close confirmation** — A confirmation before closing the window, flushing all project layouts to prevent accidental loss.
- **Update check** — Fetches the GitHub Release on startup and shows a new-version hint in the title bar.
- **Bilingual UI (English / 中文)** — A one-click language toggle under "Settings → System" instantly re-renders the entire interface; the language is auto-detected from the system on first launch and remembered across restarts. Every page and feature is fully translated, with a lightweight built-in i18n layer (no extra runtime dependency).
- **Settings center** — A unified SettingsModal managing all toggles: theme, fonts, shells, AI notifications, and more.

## Tech Stack

| Layer | Technology |
|---|---|
| Framework | Tauri v2 (Rust backend + WebView frontend) |
| Frontend | React 19 + TypeScript 5.8 + Tailwind CSS v4 + Vite 7 |
| Terminal | xterm.js v6 (WebGL addon, Canvas fallback) |
| State | Zustand (single global store) |
| Layout | Allotment (three-column main layout + recursive SplitNode tree) |
| PTY | portable-pty 0.8 |
| Git | git2 0.19 |
| File watching | notify 7 + ignore 0.4 (.gitignore filtering) |
| Tauri plugins | `window-state` · `clipboard-manager` · `dialog` · `opener` |
| Test coverage | 155 Rust unit tests (pty / fs / config / hook / cc-connect) |

## Getting Started

### Direct Download

Head to the [Releases](https://github.com/dreamlonglll/mini-term/releases) page to download the latest installer.

> **Platform support note**
> - **Windows** — The primary supported platform with guaranteed usability; daily development and testing all happen on Windows.
> - **macOS / Linux** — Supported at the code level (Tauri bundle targets = `all`), but **not well polished**, lacking thorough refinement; Issue reports are welcome.

#### macOS Installation Note

After downloading the `.dmg` and double-clicking to open it, if the system shows **"Mini-Term" is damaged and can't be opened. You should move it to the Bin**, the file is not actually corrupted — the Release artifact simply isn't signed with an Apple Developer ID and is rejected by Gatekeeper due to the quarantine flag.

Drag the `.app` into `/Applications`, then run this once in a terminal to lift the restriction:

```bash
xattr -cr /Applications/Mini-Term.app
```

After that it launches normally on double-click. You'll need to run it again after each version upgrade.

### Build from Source

#### Prerequisites

- [Node.js](https://nodejs.org/) >= 18
- [Rust](https://www.rust-lang.org/tools/install) >= 1.70
- [Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/)

#### Install & Run

```bash
# Clone the repo
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term

# Install dependencies
npm install

# Start the full Tauri dev environment (frontend + backend)
npm run tauri dev

# Build a release bundle
npm run tauri build
```

## Project Structure

```
mini-term/
├── src/                          # Frontend source
│   ├── App.tsx                   # Three-column main layout entry + window events
│   ├── store.ts                  # Zustand global state + persistence
│   ├── types.ts                  # Type definitions (Pane / Tab / Project / SplitNode ...)
│   ├── styles.css                # Global styles + CSS variables (Warm Carbon)
│   ├── components/
│   │   ├── ProjectList.tsx       # Project list + nested groups + DONE badge
│   │   ├── SessionList.tsx       # AI session history list (Claude / Codex)
│   │   ├── FileTree.tsx          # File directory tree + Git status + create / rename
│   │   ├── TerminalArea.tsx      # Tab management + split-tree operations
│   │   ├── SplitLayout.tsx       # Recursively renders the SplitNode tree
│   │   ├── TerminalInstance.tsx  # xterm.js instance + context menu + file drop
│   │   ├── PaneGroup.tsx         # Split group container
│   │   ├── MarkerList.tsx        # AI task marker dropdown
│   │   ├── GitHistory.tsx        # Git repo tree + commit history + Pull / Push
│   │   ├── GitHistoryContent.tsx # Git commit history content rendering
│   │   ├── GitChanges.tsx        # Source control panel (stage / unstage / commit)
│   │   ├── CommitDiffModal.tsx   # Commit diff viewer
│   │   ├── DiffModal.tsx         # Working-tree file diff viewer
│   │   ├── SearchModal.tsx       # Global file search dialog
│   │   ├── FileViewerModal.tsx   # File content viewer
│   │   ├── SessionViewerModal.tsx # AI session content viewer (Markdown rendering)
│   │   ├── SettingsModal.tsx     # Settings dialog (theme / font / shell / AI notify / hook)
│   │   ├── ToastContainer.tsx    # AI completion toast notifications
│   │   ├── ActivityBar.tsx       # Activity Bar sidebar (panel visibility + AI status badge)
│   │   ├── DoneTag.tsx           # Project list DONE badge
│   │   └── StatusDot.tsx         # Status indicator dot
│   ├── hooks/
│   │   ├── useTauriEvent.ts      # Tauri event subscription wrapper
│   │   ├── useAiSubmitMarker.ts  # AI session Enter marker
│   │   ├── useExternalFileDrop.ts # System explorer file drop onto terminal
│   │   └── useMarkerHotkeys.ts   # Marker-jump shortcuts
│   └── utils/
│       ├── contextMenu.ts        # Context menu DOM implementation
│       ├── dragState.ts          # Project tree drag state
│       ├── fileDragState.ts      # File-drop-onto-terminal state management
│       ├── projectTree.ts        # Recursive project tree operations
│       ├── terminalCache.ts      # xterm cache + copy/paste
│       ├── projectDataCache.ts   # FileTree / GitHistory per-project data cache
│       ├── themeManager.ts       # Theme switching + system color watching
│       └── updateChecker.ts      # GitHub Release version check
├── src-tauri/                    # Rust backend (Tauri app + shared crate + sidecars)
│   ├── src/
│   │   ├── lib.rs                # Tauri init + command / plugin registration
│   │   ├── pty.rs                # PTY lifecycle + AI session detection
│   │   ├── process_monitor.rs    # Child process status polling (500ms) + hook-first
│   │   ├── config.rs             # Config persistence + version migration
│   │   ├── fs.rs                 # Directory list / watch / create / rename / delete
│   │   ├── git.rs                # Git operations (status / diff / log / pull / push)
│   │   ├── search.rs             # Global file search (filename + content, streaming)
│   │   ├── ai_sessions.rs        # Claude / Codex session record reading (local + WSL UNC)
│   │   ├── wsl_distros.rs        # WSL distro enumeration (registry Lxss, no wsl.exe spawn)
│   │   ├── hook_server.rs        # Hook HTTP server (receives AI tool events)
│   │   ├── hook_registry.rs      # Hook register / unregister (Claude Code + Codex)
│   │   ├── ssh.rs                # SSH connection management + password auto-fill / key handling
│   │   └── ssh_mcp_registry.rs   # Per-project SSH MCP enablement (writes .mcp.json / Codex config)
│   ├── mt-core/                  # Shared library crate without tauri deps (SSH types / config / keys)
│   └── mt-sidecars/src/bin/      # Standalone sidecar crate (no tauri-build dependency)
│       ├── miniterm-hook.rs      # Hook CLI tool (called by AI tool hooks)
│       └── mt-ssh-mcp.rs         # SSH MCP server (rmcp stdio, for terminal AI agents)
├── scripts/
│   └── stage-sidecars.mjs        # Builds sidecars and stages them per-triple as Tauri externalBin
└── package.json
```

## Architecture Overview

### Data Flow

```
User keystroke → xterm.onData → invoke('write_pty') → Rust PTY writer
Rust PTY reader → 16ms batch buffer → emit('pty-output') → term.write()
Process exit       → emit('pty-exit')          → store.updatePaneStatusByPty('error')
Process monitor 500ms → emit('pty-status-change') → StatusDot update
File change notify  → emit('fs-change')          → FileTree refresh
ai-working → ai-idle → Toast + DONE Tag + requestUserAttention
```

### Tauri Interface Overview

- **Commands (61)** — PTY: `create_pty` · `write_pty` · `resize_pty` · `kill_pty`; FS: `list_directory` · `read_file_content` · `watch_directory` · `unwatch_directory` · `create_file` · `create_directory` · `rename_entry` · `delete_entry` · `filter_directories`; Search: `start_search` · `cancel_search`; Git: `get_git_status` · `get_git_diff` · `discover_git_repos` · `get_git_log` · `get_repo_branches` · `get_commit_files` · `get_commit_file_diff` · `git_pull` · `git_push` · `get_changes_status` · `git_stage` · `git_unstage` · `git_stage_all` · `git_unstage_all` · `git_commit` · `git_discard_file`; Config: `load_config` · `save_config`; Editor: `open_in_editor` · `open_path_with_default_app`; Clipboard: `read_clipboard_image` · `save_clipboard_text`; AI: `get_ai_sessions` · `get_wsl_ai_sessions` · `get_ai_session_content`; WSL: `list_wsl_distros`; Hook: `register_ai_hooks` · `unregister_ai_hooks` · `get_hook_config_snippet` · `get_hook_status` · `toggle_hook_server`; SSH: `arm_ssh_autofill` · `prepare_ssh_key`; SSH MCP: `enable_ssh_mcp` · `disable_ssh_mcp`; Theme: `set_window_dark_mode`; cc-connect: `cc_connect_probe` · `cc_connect_read_token` · `cc_connect_config_path` · `cc_connect_start` · `cc_connect_stop` · `cc_connect_restart` · `cc_connect_list_projects` · `cc_connect_import_project` · `cc_connect_import_projects` · `cc_connect_unlink_project`
- **Events (backend → frontend)** — `pty-output` · `pty-exit` · `pty-status-change` · `fs-change` · `search-results` · `search-complete`

### Status Priority

Terminal pane status is aggregated from leaf nodes up to the tab and project levels:

```
error > ai-working > ai-idle > idle
```

### Layout Model

```
App
├── ActivityBar (persistent far-left, panel visibility toggles + AI status badge)
├── Allotment three columns
│   ├── Left: ProjectList (projects + groups + sessions + DONE badge)
│   ├── Middle: FileTree (directory browsing + Git status + file operations)
│   └── Right
    ├── TabBar (tab management)
    ├── SplitLayout (recursive SplitNode tree)
    │   └── TerminalInstance × N (xterm.js + context menu)
    └── GitHistory (repo tree + commit history + Pull/Push)

ToastContainer floats at the bottom-right; SettingsModal overlays globally.
```

## Recommended Dev Environment

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Contributing

Issues and PRs are welcome. External contributions are merged after functional verification and a security review.

Before submitting, please run:

```bash
# Frontend type check
npm run build

# Rust tests & build
cd src-tauri && cargo test && cargo build
```

## Community

Learn AI, join the L site — [LinuxDO](https://linux.do/)

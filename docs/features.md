<p align="center">
  <img src="../src-tauri/icons/icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>A desktop terminal manager built for the AI era</strong><br>
  Powered by Tauri v2 · Multi-project · Multi-tab · Split-pane layout · AI process awareness · SSH remote projects · Git worktree management · Watch your AI from your phone
</p>

<p align="center">
  <a href="features.zh-CN.md">简体中文</a> · <strong>Full feature list · English</strong><br>
  <a href="../README.en.md">← Back to the project home</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-0.8.3-blue" alt="version">
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

![Main UI](screenshots/main.png)

## Features

### Terminal Core

- **Multi-tab management** — A dedicated tab per project, drag to reorder, status icons at a glance.
- **Recursive splitting** — Arbitrarily nested horizontal / vertical splits, drag to adjust ratios via Allotment.
- **High-performance rendering** — xterm.js v6 + WebGL acceleration, automatic fallback to Canvas; minimum contrast is enforced, fixing Claude's prompt text being nearly invisible against a dark background.
- **100k-line scrollback buffer** — Keeps up to 100,000 normal-buffer lines while honoring standard CSI 3J (ED3) globally, so applications such as Codex can discard transient output and replay a folded transcript, while `/clear` can truly purge old history; alternate-screen switching remains intercepted so TUI overlays stay in the main buffer with a usable scrollbar. On Windows, mini-term bundles and preloads a pinned official ConPTY compatibility runtime (with a system-ConPTY fallback if validation fails) to keep Codex scrolling and transcript folding consistent across Windows versions.
- **Terminal caching** — Switching projects / tabs / panes never rebuilds the xterm instance, so existing content is preserved; lazy startup creates a PTY only for the currently visible pane, avoiding the slowdown of spawning more terminals the more history projects you have.
- **Project-switch caching** — FileTree / GitHistory data is cached per project, so switching back to a visited project renders with zero latency; directory loading and Git status run in parallel, and Git repo scan results are cached for 30 seconds.
- **Copy & paste** — `Ctrl+Shift+C/V` (macOS `⌘+Shift+C/V`) shortcuts + context menu, with "Copy" auto-greyed when nothing is selected; an optional "Smart `Ctrl+C/V`" mode (copy when there's a selection, interrupt the program when there isn't, and `Ctrl+V` pastes directly); on Windows, large multi-line pastes are chunked to prevent ConPTY from dropping lines.
- **Long-text paste** — When clipboard text is ≥10 lines or ≥2000 chars, it is automatically saved to a temporary `.txt` and a quoted file path is pasted instead, avoiding the performance and paste-bracket issues of feeding huge content straight to AI tools.
- **Image paste** — Detects screenshots in the clipboard, saves them to a temporary PNG via the Win32 API, and pastes a quoted path; compatible with non-standard formats such as PinPix.
- **Remote / WSL paste lands where the agent can read it** — Both "save to a file, paste the path" features above automatically remap their destination in remote terminals: SSH remote projects upload the file over SFTP and paste the **remote** path (default `<project root>/.mini-term/pasted`, inside the project so agents need no extra permission; configurable to `/tmp/mini-term`, `~/uploads`, etc., and a self-ignoring `.gitignore` is written so your `git status` stays clean), while WSL projects rewrite `C:\...` into `/mnt/c/...` (no upload needed). Upload failures raise an explicit toast instead of pasting a local path the remote host cannot read.
- **File drag & drop** — Dragging a file from the file tree or system file explorer onto the terminal inserts its quoted absolute path, targeting the exact split pane and handling paths with spaces.
- **Multiple shell profiles** — Windows (cmd / powershell / pwsh), macOS (zsh / bash), Linux (bash / sh) and more, freely added or removed.

### SSH Connections

- **Connection management** — The top-bar "SSH" button opens a management dialog with a two-pane layout (group list on the left, connections of the selected group on the right) to add / edit / delete SSH connections, with host / port / username / password / private key / group fields, persisted to the config file. The "Link SSH" and "Add remote project" dialogs share the same structure (one group-bucketing implementation, collapsible groups in the All view, and select-all / clear-all acting only on currently visible connections), and deleting a connection asks for confirmation first, warning that the stored password and private-key path will be lost.
- **Quick connect** — A right-click "SSH Connect" submenu inside the terminal lists saved connections by group; selecting one assembles the `ssh` command and launches the session right in the current terminal.
- **Password auto-fill** — For connections with a saved password, the backend scans PTY output for the password prompt and writes the password back automatically, once per session, stopping on a wrong password to avoid hammering the server with bad credentials.
- **Private-key permission handling** — When connecting with a private key, the key is copied to a permission-tightened temporary copy (Windows `icacls` / Unix `0600`) to bypass OpenSSH's "UNPROTECTED PRIVATE KEY FILE" rejection, without modifying your original key file.
- **Advanced capabilities** — Key-file login (`ssh -i`) and connection grouping: right-click to create / rename / dissolve groups (empty groups persist), drag a connection onto a group to move it, and pick existing groups from a dropdown in the edit form.
- **SSH MCP Server** — Exposes saved SSH connections as MCP tools to AI agents running in the terminal (Claude Code / Codex). The project right-click "Link SSH" menu enables them per project and limits visibility to the selected connections; the built-in `mt-ssh-mcp` sidecar (a stdio MCP server based on the official rmcp) provides `ssh_list_connections`, `ssh_exec`, `ssh_upload` and `ssh_download`, where `ssh_exec` reuses password / private-key auth with timeouts, output capping, and an audit log; `ssh_upload` / `ssh_download` transfer single files over SFTP (streamed in chunks for constant memory, so large files work, unlike base64-echoing through `ssh_exec`), download writes straight to a local path on disk, and a hard guard refuses to ever transfer mini-term's own `config.json` (which holds all saved SSH credentials); enabling / disabling writes idempotently into Claude's `.mcp.json` and Codex's `.codex/config.toml` using named markers. **Since v0.4.10 the sidecar maintains an in-process SSH session pool** (russh 0.61 + tokio): the first call to a connection does one TCP handshake + auth (~seconds), and subsequent commands cost only an RTT; sessions are recycled after 10 minutes idle or 2 hours max, and gracefully `disconnect` when the sidecar exits.
- **SSH remote projects** — Add a directory on a remote server directly as a mini-term project: the "Add Remote Project" dialog picks a saved SSH connection and takes a remote POSIX path, validating that the directory exists over SSH before saving; the file tree lazy-loads over SFTP (an inline loading spinner on expand, manual refresh, root `.gitignore` filtering); the terminal connects via `ssh -t` and lands straight in the project directory, with a one-click reconnect overlay after a disconnection; the Sessions panel merges the remote machine's Claude / Codex sessions chronologically, with content viewing supported; deleting the referenced connection shows the project in a "broken-link" state rather than failing silently; under the hood it shares the extracted `mt-ssh` crate (persistent russh session pool + SFTP primitives) with the SSH MCP sidecar, and remote cache keys mix in the connection id so identical paths on two servers never cross-contaminate.

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

- **Hook event system** — Integrates the official Claude Code / Codex Hook APIs to receive AI tool events (SessionStart / End, ToolUse, etc.), which is more precise and timely than process polling; the built-in `miniterm-hook` CLI is called by the hook system to POST events to a local server; the settings UI registers / unregisters the hook config with one click, merging rather than overwriting your existing hooks. Codex permission requests stay in `ai-working` through approval and tool execution, avoiding premature completion notifications.
- **Real-time status detection** — Hook-first with a 500ms process-polling fallback, auto-detecting Claude / Codex / OpenCode and showing idle / working / error states.
- **Status aggregation** — Aggregated layer by layer from pane → tab → project, with priority `error > ai-working > ai-idle > idle`.
- **Completion notification trio** — Fires the moment an AI task goes working → idle:
  - A bottom-right toast desktop notification (only for inactive projects, deduplicated per project).
  - A DONE badge in the project list, cleared on click.
  - Taskbar flashing (Windows) / Dock bouncing (macOS), triggered only when the window is unfocused.
  - A notification sound (a default tone synthesized via the Web Audio API, with support for a custom audio file).
  - All notification toggles are independently configurable, managed on a dedicated "AI Completion Notifications" page in the settings center.
- **Session enter/exit detection** — Recognizes entering AI via command echo; recognizes exit via a double `Ctrl+C` / `Ctrl+D` or `exit` / `quit` / `:quit` / `/logout`.
- **Session history** — Reads local Claude / Codex history records, with a right-click to copy the resume command for quick continuation; the first screen renders only 20 entries, with a "Load more" button at the bottom to expand on demand (no longer triggered by scrolling).
- **Session viewer** — A right-click "View" shows the full conversation, with User as plain text and Assistant rendered as Markdown (external links open in the system default browser after a confirmation prompt), supporting `Ctrl+F` search highlighting and quick navigation between User messages.
- **WSL sessions** — Reads Claude / Codex session history inside WSL distros directly from Windows (no `wsl.exe` spawn — via `\\wsl$` UNC plus registry-based distro enumeration): WSL-rooted projects auto-derive the distro and path with zero configuration; Windows-path projects pick a distro via the right-click "WSL Sessions" submenu and are scanned through `/mnt` path mapping, with in-session cwd verification to prevent cross-project mixing; WSL sessions merge chronologically with local ones under a WSL badge, a header spinner shows while loading, and viewing session content is supported too.
- **AI task markers** — Each time the user presses Enter inside an AI session, a marker is dropped in xterm; the ⚑ button at the tab's top-right drops down the list of past submissions, and clicking one or pressing `Ctrl+Shift+↑/↓` (macOS `⌘+Shift+↑/↓`) jumps between markers, briefly highlighting the target line.

### Mobile Client + Self-Hosted Relay

Watch the AI running on your desktop from your phone while you're out, and send it commands directly.

**Prerequisite**: you need your own publicly reachable server to run the relay (1 vCPU / 1 GB is plenty, one Docker command to start, plus a domain pointed at it for TLS — see the [deployment guide](deploy-relay.md)).

- **Connect and pair in one place** — Fill in the relay address in the top-bar "Mobile" panel → save & connect → generate a pairing QR code, all in a single panel. Scanning with your phone camera opens the PWA and pairs automatically; the code is single-use (valid for 10 minutes), pairing a new device replaces the old one, and "Reset pairing" revokes every credential instantly.
- **Active AI session list** — The phone shows running Claude / Codex sessions grouped by project, with status lights that add, remove, and change color in real time alongside the desktop; when the desktop goes offline a top banner appears and the list greys out, clearing automatically on reconnect.
- **Start a new session from your phone** — Tap **+** → pick a project → pick an AI launcher, and the desktop opens a terminal tab in that project in the background and brings the agent up; once the session is really running the phone enters its mirror automatically, without disturbing whatever you are looking at on the desktop. Projects are listed with the desktop's group hierarchy and can be collapsed. Launchers are named entries configured on the desktop — the phone references them by id and only ever sees the name, so **the command text never passes through the phone or the relay**.
- **Rename sessions** — Give a session a name you will recognise, from the ✎ on a list row or the title on the mirror page; it shows up on the desktop terminal tab as well. Leave it empty to restore the default name.
- **Conversation mirror (read-only)** — Tap into any session to follow the conversation live, with AI replies rendered as Markdown and desktop input shown verbatim; scrolling to the top pages in older messages. Mirror binding resolves the session identity through hooks down to the exact pane, so multiple AI sessions running in the same project never cross-contaminate.
- **Mobile commands** — The input box at the bottom of the mirror page writes text straight through to the corresponding desktop terminal (equivalent to typing it yourself and pressing Enter), with an immediate receipt and an explicit failure reason; when the desktop is offline the relay rejects the command outright rather than storing and forwarding it.
- **The relay forwards, never persists** — The relay server stores no message bodies and logs metadata only (a subprocess-level automated test asserts zero file residue across the full flow); it ships with a three-stage Dockerfile and a compose example to build and run from source in one command — reverse proxy + TLS setup in the [deployment guide](deploy-relay.md).
- **PWA experience** — "Add to Home Screen" runs it as a standalone window, with exponential-backoff reconnection that automatically restores subscriptions, and the same bilingual (English / 中文) layer as the desktop app.

### Project Management

- **Project list** — Manage multiple project directories in the left sidebar, switch workspaces in one click, and restore the last active project on restart.
- **Drag to add projects** — Drag a folder from the file explorer onto the project list to add it quickly, with automatic detection of files / folders / duplicate projects and visual feedback.
- **Nested groups** — Up to 3 levels of project grouping, drag to reorder, collapse / expand, with a group context menu to add either a local project or a remote SSH project directly into that group (a collapsed group expands automatically). "Delete group" now asks for confirmation first, explaining that the projects inside move up one level rather than being deleted; "Move to group" expands the group tree level by level as submenus, marking the current group with a ✓ and greying it out, with over-depth groups unselectable.
- **Worktree sub-projects** — A worktree turned into a project is mounted beneath its main project as a sub-project (indented, following the group), and can be dragged out or detached via "Detach from parent" to return to the top level; deleting a parent project promotes its sub-projects in place instead of losing them. The project list shows a ⎇ branch badge for worktree projects, and the repo list and Changes dropdown label worktree entries as well.
- **File tree** — An integrated directory browser with natural sorting (V1 → V2 → V10 rather than lexicographic), nested `.gitignore` greying (ignore rules and `!pattern` allowlists at every sub-directory level take effect, consistent with git behavior), and live refresh via `notify` file watching.
- **File operations** — Create / rename / delete files and folders and view contents inside the file tree (Markdown rendering supports HTML tags and external images, external links open in the system default browser after a confirmation prompt, image formats are shown directly, HTML files preview in an iframe with relative-path resources auto-resolved, and binary / oversized files get a friendly notice).
- **Open in external editor** — A button at the top-right of the file tree opens the current project in your configured editor (VS Code by default), with the path customizable under "Settings → System → External Editor"; files can be opened with the system default app.
- **Project-level environment variables** — The project context menu "Environment Variables…" opens a management dialog with a row-level `[enable checkbox][key][value][✕]` layout, injecting per-project variables into the PTY child process when starting that project's terminal; strict POSIX validation (key matches `^[A-Za-z_][A-Za-z0-9_]*$`, no `MINITERM_` prefix, no `WSLENV`, no duplicates within a project, and value forbids `\n/\r/\0`); the Rust side adds a defensive `MINITERM_`-prefix + `WSLENV` filter, so even hand-editing `config.json` to bypass frontend validation cannot break the hook protocol or WSLENV concatenation; under WSL projects, variables pass through to Linux bash via the WSLENV mechanism (`/u` is one-way without path translation; an `export` of the same name in `~/.bashrc` will override).

### Git Integration

- **File status** — The file tree shows Git status colors (modified / added / deleted / conflict).
- **Change diff** — A detailed diff of working-tree file changes, parsed at the hunk/line level, with side-by-side / inline dual views; side-by-side mode supports dragging to adjust the split ratio, and the font size follows the terminal font setting.
- **Commit history** — Browse the repo's commit log with cursor-based pagination (30 entries by default).
- **Branch topology graph** — Each history row draws an SVG topology graph on the left, laying out branch, merge, and pass-through lines by lane, coloring nodes per lane and marking merge commits with a filled dot inside an outer ring; merge-in lines use the branch's own color as a Bézier curve that gradient-blends into the mainline at its root. The backend revwalk appends TOPOLOGICAL sorting so clock skew or a rebase can't place a parent after its child and break the lines, and a commit row is only labeled with the branches this repo itself has checked out, rather than hanging every other worktree / remote branch on it.
- **Commit diff** — View the file changes of any commit, switching file by file.
- **Branch info** — Local / remote branch lists.
- **Source control panel** — A VS Code-style Changes panel grouping Staged / Changes / Untracked, supporting per-file and bulk stage / unstage / discard, `Ctrl+Enter` to commit quickly, and toggling between list and tree views.
- **Pull / Push** — In-row buttons sync with the remote in one click, with a refresh button to reload the commit log and branch info.
- **Multi-repo discovery** — Automatically scans all Git repos under the project directory (recursing 5 levels, skipping `node_modules` etc.).
- **Worktree management** — Right-click a project or a repo row in the Git panel to open the "Worktree management" dialog: list every worktree, create one from an existing branch or a new branch, remove it (force optional), and prune stale entries, with the repo list refreshing immediately after any change; a worktree can be turned into a project in one click or opened directly in a terminal, and panes support a working-directory override that persists with the layout and is inherited by splits. When the project root itself isn't a repo, it scans downward for sub-repos and groups them by main worktree into a list whose group headers are checkable (multi-select / select-all), creating one worktree per checked repo in a single action — the branch dropdown then offers the intersection of all repos' branches, the path field becomes a parent directory previewing the `<repo>-<branch>` landing spot, and failures are listed per repo.

![Git integration](screenshots/git.png)

### Appearance & Configuration

- **Icon sidebar + three-column layout** — A persistent icon bar on the far left (collapse middle column / Sessions / Git / Settings / SSH); the middle column stacks Projects over Files and collapses as a whole; the terminal sits on the right. Sessions / Git are now floating drawers that slide out from the right edge over the terminal (mutually exclusive single panel, left-edge drag to resize with persisted width, ✕ to close), with a blue vertical bar indicating the active state.
- **Three theme modes** — Auto (follows the system) / Light / Dark, with Dark based on a Warm Carbon palette and a custom CSS-variable system; the native Windows title bar (DWM Immersive Dark Mode) follows the theme automatically, with no first-frame light flash for dark-mode users on startup.
- **Blueprint skin** — An optional sci-fi Blueprint skin with a grid background + corner markers + glow effects, supporting both dark and light modes, with the terminal palette switching in sync.
- **Independent font tuning** — The UI and terminal font sizes (10-20px) / families are adjustable separately, and the terminal can optionally follow the UI theme.
- **Ligatures** — A terminal ligature toggle that composes glyphs like `==` `=>` `!=` `->` when enabled, requiring a font with a calt table (Fira Code / JetBrains Mono); fully supported on Windows, while macOS / Linux use a 60-entry Iosevka fallback due to webview API limitations.
- **Layout persistence** — Split ratios, tabs, and window size / position are saved automatically and restored on restart (`tauri-plugin-window-state`).
- **Close confirmation** — Closing the window takes stock of AI sessions only (panes in ai-working / ai-idle); plain shell terminals no longer count, and the confirmation appears only when AI sessions exist, listing their names. All project layouts are flushed either way.
- **Update check** — Fetches the GitHub Release on startup; when a new version is available a highlighted hint appears on the icon sidebar (click to download), and the version number is written into the native window title.
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
| Mobile relay | axum + tokio WebSocket relay service (`relay-server/`) · React + TS + Vite PWA (`mobile/`) |
| Test coverage | 419 Rust tests = 381 desktop (tauri-app 277 + mt-core 38 + mt-ssh 26 + mt-sidecars 40) + 38 relay-server (protocol & routing); plus 19 Node tests |

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

- [Node.js](https://nodejs.org/) >= 20.19 (or >= 22.12) — required by Vite 7's `engines`; CI runs Node 22
- [Rust](https://www.rust-lang.org/tools/install) >= 1.85 — set by russh 0.61 (Tauri v2 itself only needs 1.77.2)
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
│   ├── App.tsx                   # Main layout entry (ActivityBar + 2-column Allotment + drawer) + window events
│   ├── store.ts                  # Zustand global state + persistence
│   ├── types.ts                  # Type definitions (Pane / Tab / Project / SplitNode ...)
│   ├── styles.css                # Global styles + CSS variables (Warm Carbon)
│   ├── components/
│   │   ├── ActivityBar.tsx       # Far-left persistent icon rail (panel toggles + AI status badge)
│   │   ├── RightDrawer.tsx       # Floating drawer sliding in from the right (Sessions / Git)
│   │   ├── ProjectList.tsx       # Project list + nested groups + DONE badge
│   │   ├── AddRemoteProjectModal.tsx # Add SSH remote project dialog (connection pick + remote path validation)
│   │   ├── ProjectEnvVarsModal.tsx   # Per-project environment variables dialog (POSIX validation)
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
│   │   ├── SshModal.tsx          # SSH connection manager (groups + connection CRUD)
│   │   ├── SshAssocModal.tsx     # Per-project SSH association (enables MCP, scopes visibility)
│   │   ├── MobileRelayModal.tsx  # "Mobile" panel (relay address / status / pairing QR / AI launchers)
│   │   ├── AiLauncherSection.tsx # AI launcher CRUD (name / shell / command + command-detection warning)
│   │   ├── RelayStatusBadge.tsx  # Relay connection status badge
│   │   ├── SettingsModal.tsx     # Settings dialog (theme / font / shell / AI notify / hook)
│   │   ├── LanguageToggle.tsx    # Chinese / English switcher
│   │   ├── ToastContainer.tsx    # AI completion toast notifications
│   │   ├── DoneTag.tsx           # Project list DONE badge
│   │   └── StatusDot.tsx         # Status indicator dot
│   ├── hooks/
│   │   ├── useTauriEvent.ts      # Tauri event subscription wrapper
│   │   ├── useAiSubmitMarker.ts  # AI session Enter marker
│   │   ├── useExternalFileDrop.ts # System explorer file drop onto terminal
│   │   └── useMarkerHotkeys.ts   # Marker-jump shortcuts
│   ├── i18n/                     # In-house lightweight i18n (locales/<ns>.ts dictionaries + useT())
│   └── utils/                    # Excerpt below; 24 files in total
│       ├── contextMenu.ts        # Context menu DOM implementation
│       ├── terminalCache.ts      # xterm cache + copy/paste + long-text / image paste
│       ├── pastePath.ts          # Paste destination resolution (local / WSL rewrite / SSH remote upload)
│       ├── wslPath.ts            # WSL UNC detection + Windows path to /mnt form
│       ├── terminalSnapshot.ts   # Terminal content snapshots (for layout restore)
│       ├── projectTree.ts        # Recursive project tree operations
│       ├── projectDataCache.ts   # FileTree / GitHistory per-project data cache
│       ├── projectEnv.ts         # Per-project environment variable validation
│       ├── remoteProject.ts      # SSH remote project helpers (detection / broken-link check / remote PTY creation)
│       ├── wslPath.ts            # WSL UNC path parsing and display
│       ├── mobileSessionSync.ts  # Pushes project + active AI session snapshots (with group hierarchy)
│       ├── mobileStartSession.ts # Desktop landing for phone-started sessions (pane + launch command)
│       ├── ptyWriteQueue.ts      # PTY write queue (chunked large pastes)
│       ├── themeManager.ts       # Theme switching + system color watching
│       └── updateChecker.ts      # GitHub Release version check
├── src-tauri/                    # Rust backend (Tauri app + shared crate + sidecars)
│   ├── src/
│   │   ├── lib.rs                # Tauri init + command / plugin registration
│   │   ├── pty.rs                # PTY lifecycle + AI session detection
│   │   ├── conpty_bootstrap.rs   # Preloads the bundled Windows ConPTY runtime (falls back to system ConPTY)
│   │   ├── process_monitor.rs    # Child process status polling (500ms) + hook-first
│   │   ├── config.rs             # Config persistence + version migration
│   │   ├── fs.rs                 # Directory list / watch / create / rename / delete
│   │   ├── git.rs                # Git operations (status / diff / log / pull / push)
│   │   ├── search.rs             # Global file search (filename + content, streaming)
│   │   ├── clipboard.rs          # Clipboard image reading + long-text spill to temp file
│   │   ├── editor.rs             # Open in external editor / system default app
│   │   ├── ai_sessions.rs        # Claude / Codex session record reading (local + WSL UNC)
│   │   ├── wsl_distros.rs        # WSL distro enumeration (registry Lxss, no wsl.exe spawn)
│   │   ├── hook_server.rs        # Hook HTTP server (receives AI tool events)
│   │   ├── hook_registry.rs      # Hook register / unregister (Claude Code + Codex)
│   │   ├── ssh.rs                # SSH connection management + password auto-fill / key handling
│   │   ├── remote_ssh.rs         # SSH remote projects (SFTP dir listing / dir validation / remote session reading)
│   │   ├── ssh_mcp_registry.rs   # Per-project SSH MCP enablement (writes .mcp.json / Codex config)
│   │   ├── mobile_relay.rs       # Mobile relay (outbound WSS link / pairing / snapshots / commands / start session / rename)
│   │   ├── mobile_mirror.rs      # Conversation mirror (incremental session JSONL parsing + pagination)
│   │   ├── window_theme.rs       # Native Windows title bar dark mode (DWM Immersive Dark Mode)
│   │   └── window_input_recovery.rs # Recovery from stuck window input focus
│   ├── mt-core/                  # Shared library crate without tauri deps (SSH types / config / keys)
│   ├── mt-ssh/                   # Shared SSH crate (persistent russh session pool + SFTP primitives, used by both the app and sidecars)
│   └── mt-sidecars/src/bin/      # Standalone sidecar crate (no tauri-build dependency)
│       ├── miniterm-hook.rs      # Hook CLI tool (called by AI tool hooks)
│       └── mt-ssh-mcp.rs         # SSH MCP server (rmcp stdio, for terminal AI agents)
├── relay-server/                 # Self-hosted relay service (standalone Rust workspace)
│   ├── protocol/                 # Protocol message crate shared by desktop and relay (JSON over WebSocket)
│   ├── server/                   # axum relay service (forward-only, no persistence + PWA static hosting)
│   └── docker-compose.yml        # Build and run from source in one command
├── mobile/                       # Mobile PWA (React + TS + Vite — pairing / list / mirror / commands / start / rename)
├── scripts/
│   ├── stage-sidecars.mjs        # Builds sidecars and stages them per-triple as Tauri externalBin
│   └── stage-conpty.mjs          # Downloads, verifies and stages the pinned ConPTY runtime (Windows)
├── tests/                        # Node-side tests (19: ConPTY bundling / TUI scrollback / layout restore / theme compat / WSL path ...)
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

- **Commands (69)** — PTY: `create_pty` · `write_pty` · `resize_pty` · `kill_pty`; FS: `list_directory` · `read_file_content` · `watch_directory` · `unwatch_directory` · `create_file` · `create_directory` · `rename_entry` · `delete_entry` · `filter_directories`; Search: `start_search` · `cancel_search`; Git: `get_git_status` · `get_git_diff` · `discover_git_repos` · `get_git_log` · `get_repo_branches` · `get_commit_files` · `get_commit_file_diff` · `git_pull` · `git_push` · `get_changes_status` · `git_stage` · `git_unstage` · `git_stage_all` · `git_unstage_all` · `git_commit` · `git_discard_file` · `list_worktrees` · `add_worktree` · `remove_worktree` · `prune_worktrees` · `get_worktree_branches`; Config: `load_config` · `save_config`; Editor: `open_in_editor` · `open_path_with_default_app`; Clipboard: `read_clipboard_image` · `save_clipboard_text`; AI: `get_ai_sessions` · `get_wsl_ai_sessions` · `get_ai_session_content`; WSL: `list_wsl_distros`; Hook: `register_ai_hooks` · `unregister_ai_hooks` · `get_hook_config_snippet` · `get_hook_status` · `toggle_hook_server`; SSH: `arm_ssh_autofill` · `prepare_ssh_key`; SSH MCP: `enable_ssh_mcp` · `disable_ssh_mcp`; SSH remote: `ssh_remote_list_directory` · `ssh_remote_validate_dir` · `ssh_remote_ai_sessions` · `ssh_remote_ai_session_content` · `ssh_remote_upload_paste`; Theme: `set_window_dark_mode`; Mobile relay: `mobile_relay_apply` · `mobile_relay_status` · `mobile_relay_request_pairing_code` · `mobile_relay_reset_pairing` · `mobile_relay_update_sessions` · `mobile_relay_launchers_changed` · `mobile_relay_start_session_result` · `mobile_relay_check_launcher_command`
- **Events (12, backend → frontend)** — `pty-output` · `pty-exit` · `pty-status-change` · `ai-user-submit` (user pressed Enter inside an AI session; drives marker placement) · `fs-change` · `search-results` · `search-complete` · `wsl-shell-override` · `mobile-relay-status` · `mobile-relay-pairing-code` · `mobile-start-session` · `mobile-rename-pane`

### Status Priority

Terminal pane status is aggregated from leaf nodes up to the tab and project levels:

```
error > ai-working > ai-idle > idle
```

### Layout Model

```
App
├── ActivityBar (persistent far-left icon rail: collapse middle column / Sessions / Git /
│               Settings / SSH / Mobile + AI status badge)
└── Allotment, two columns (draggable, ratios persisted)
    ├── Middle column (collapsible as a whole · split vertically)
    │   ├── Top:    ProjectList (projects + nested groups + DONE badge)
    │   └── Bottom: FileTree (directory browsing + Git status + file operations)
    └── Right column: TerminalArea × N (one per project, only the active one display:block)
        ├── TabBar (tab management + ⚑ marker dropdown)
        └── SplitLayout (recursive SplitNode tree)
            └── TerminalInstance × N (xterm.js + context menu)

RightDrawer slides in from the right edge and floats above the terminal (Sessions / Git,
mutually exclusive, left edge draggable to resize, width persisted).
ToastContainer floats at the bottom-right; SettingsModal / SshModal / MobileRelayModal overlay globally.
```

## Recommended Dev Environment

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Contributing

Issues and PRs are welcome. External contributions are merged after functional verification and a security review.

Before submitting, please run:

```bash
# Frontend type check (tsc + vite build)
npm run build

# Node-side tests (19)
node --test "tests/*.test.cjs"

# Desktop Rust tests (381)
# Note: mt-core / mt-ssh / mt-sidecars are standalone crates, not workspace members.
# Running `cd src-tauri && cargo test` alone only covers tauri-app's 277 — the other
# three need their manifests specified explicitly.
cd src-tauri
cargo test                                        # tauri-app     277
cargo test --manifest-path mt-core/Cargo.toml     # mt-core        38
cargo test --manifest-path mt-ssh/Cargo.toml      # mt-ssh         26
cargo test --manifest-path mt-sidecars/Cargo.toml # mt-sidecars    40
cargo build

# Relay server tests (38, standalone workspace)
cd ../relay-server && cargo test
```

## Community

Learn AI, join the L site — [LinuxDO](https://linux.do/)

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
  <img src="https://img.shields.io/badge/version-0.12.1-blue" alt="version">
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
- **Configurable scrollback buffer** — The number of retained normal-buffer lines is adjustable in Settings (10,000 by default; lowering it takes effect immediately and frees the memory). xterm allocates per line by column count, so the old hard-coded 100,000 meant a single terminal could hold 200+ MB — and terminals are only disposed when their pane closes, so enough projects and splits would push the renderer to OOM. Standard CSI 3J (ED3) is still honored globally, so applications such as Codex can discard transient output and replay a folded transcript, while `/clear` can truly purge old history; alternate-screen switching remains intercepted so TUI overlays stay in the main buffer with a usable scrollbar. On Windows, mini-term bundles and preloads a pinned official ConPTY compatibility runtime (with a system-ConPTY fallback if validation fails) to keep Codex scrolling and transcript folding consistent across Windows versions.
- **Terminal caching** — Switching projects / tabs / panes never rebuilds the xterm instance, so existing content is preserved; lazy startup creates a PTY only for the currently visible pane, avoiding the slowdown of spawning more terminals the more history projects you have.
- **Project-switch caching** — FileTree / GitHistory data is cached per project, so switching back to a visited project renders with zero latency; directory loading and Git status run in parallel, and Git repo scan results are cached for 30 seconds.
- **Copy & paste** — `Ctrl+Shift+C/V` (macOS `⌘+Shift+C/V`) shortcuts + context menu, with "Copy" auto-greyed when nothing is selected; an optional "Smart `Ctrl+C/V`" mode (copy when there's a selection, interrupt the program when there isn't, and `Ctrl+V` pastes directly); on Windows, large multi-line pastes are chunked to prevent ConPTY from dropping lines.
- **Dwell-to-copy selection** — After drag-selecting, holding the mouse still past a configurable dwell (default 1s, 0.2–60s, 0 = off) copies the selection and shows a "Copied" tip at the cursor; if the selection kept growing before mouse-up, it copies once more so the clipboard always matches the final selection.
- **Long-text paste** — When clipboard text is ≥10 lines or ≥2000 chars, it is automatically saved to a temporary `.txt` and a quoted file path is pasted instead, avoiding the performance and paste-bracket issues of feeding huge content straight to AI tools.
- **Image paste** — Detects screenshots in the clipboard, saves them to a temporary PNG via the Win32 API, and pastes a quoted path; compatible with non-standard formats such as PinPix.
- **Remote / WSL paste lands where the agent can read it** — Both "save to a file, paste the path" features above automatically remap their destination in remote terminals: SSH remote projects upload the file over SFTP and paste the **remote** path (default `<project root>/.mini-term/pasted`, inside the project so agents need no extra permission; configurable to `/tmp/mini-term`, `~/uploads`, etc., and a self-ignoring `.gitignore` is written so your `git status` stays clean), while WSL projects rewrite `C:\...` into `/mnt/c/...` (no upload needed). Upload failures raise an explicit toast instead of pasting a local path the remote host cannot read.
- **File drag & drop** — Dragging a file from the file tree or system file explorer onto the terminal inserts its quoted absolute path, targeting the exact split pane and handling paths with spaces. Press `Esc` mid-drag to cancel on the spot: no path is written to the PTY (that Esc is swallowed in the window capture phase, so it never reaches the terminal as `\x1b`), releasing the mouse doesn't degrade into a plain click that opens the file, and the hover outline is cleared along with it. Esc is only swallowed once the drag is actually active (past the 5px threshold), so Esc elsewhere still behaves normally.
- **Multiple shell profiles** — Windows (cmd / powershell / pwsh), macOS (zsh / bash), Linux (bash / sh) and more, freely added or removed.

### SSH Connections

- **Connection management** — The top-bar "SSH" button opens a management dialog with a two-pane layout (group list on the left, connections of the selected group on the right) to add / edit / delete SSH connections, with host / port / username / password / private key / group fields, persisted to the config file. The "Link SSH" and "Add remote project" dialogs share the same structure (one group-bucketing implementation, collapsible groups in the All view, and select-all / clear-all acting only on currently visible connections), and deleting a connection asks for confirmation first, warning that the stored password and private-key path will be lost.
- **Quick connect** — A right-click "SSH Connect" submenu inside the terminal lists saved connections by group; selecting one assembles the `ssh` command and launches the session right in the current terminal.
- **Password auto-fill** — For connections with a saved password, the backend scans PTY output for the password prompt and writes the password back automatically, once per session, stopping on a wrong password to avoid hammering the server with bad credentials.
- **Private-key permission handling** — When connecting with a private key, the key is copied to a permission-tightened temporary copy (Windows `icacls` / Unix `0600`) to bypass OpenSSH's "UNPROTECTED PRIVATE KEY FILE" rejection, without modifying your original key file.
- **Advanced capabilities** — Key-file login (`ssh -i`) and connection grouping: right-click to create / rename / dissolve groups (empty groups persist), drag a connection onto a group to move it, and pick existing groups from a dropdown in the edit form.
- **SSH tools for AI agents (CLI + Skill)** — Lets AI agents running in the terminal (Claude Code / Codex) operate on saved SSH connections. The project right-click "Link SSH" menu enables them per project and limits visibility to the selected connections; enabling generates two SKILL.md files (Claude / Codex variants, with the CLI's absolute path and a random per-project capability token baked in, `.gitignore` entries appended, and any legacy MCP registration migrated away automatically). The token is required on every `list` / `exec` / `upload` / `download` call; missing, blank, unknown, duplicate, or disabled-project mappings fail closed instead of exposing all connections. Generated examples cover Bash, correctly quoted WSL interop, and PowerShell's required `&` call operator. Remote stdout/stderr stream through verbatim and the remote exit code is passed through (124 = timeout, 2 = CLI error); SFTP transfers stream in chunks, credentials stay local, each call is audited, and a hard guard refuses to transfer mini-term's own credential-bearing `config.json`. A machine-wide singleton daemon holds the persistent session pool (auto-spawned on first call, drains after 10 minutes idle, swaps itself out on upgrades); Ctrl+C/client disconnect and request timeout explicitly close the SSH channel while retaining the healthy session. IPC is current-user-only and fails closed if its secure endpoint cannot be created. If the daemon is unreachable the CLI transparently falls back to an in-process connection. During the transition the `mt-ssh-mcp` MCP sidecar still ships for projects not yet migrated.
- **SSH remote projects** — Add a directory on a remote server directly as a mini-term project: the "Add Remote Project" dialog picks a saved SSH connection and takes a remote POSIX path, validating that the directory exists over SSH before saving; the file tree lazy-loads over SFTP (an inline loading spinner on expand, manual refresh, root `.gitignore` filtering); the terminal connects via `ssh -t` and lands straight in the project directory, with a one-click reconnect overlay after a disconnection; the Sessions panel merges the remote machine's Claude / Codex sessions chronologically, with content viewing supported; deleting the referenced connection shows the project in a "broken-link" state rather than failing silently; under the hood it shares the extracted `mt-ssh` crate (persistent russh session pool + SFTP primitives) with the SSH tool sidecars, and remote cache keys mix in the connection id so identical paths on two servers never cross-contaminate.

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

- **Hook event system** — Integrates the official Claude Code / Codex / Grok Build Hook APIs to receive AI tool events (SessionStart / End, ToolUse, etc.), which is more precise and timely than process polling; the built-in `miniterm-hook` CLI is called by the hook system to POST events to a local server; the settings UI registers / unregisters hooks per CLI via "injection targets" — one checkbox row each for Claude Code / Codex / Grok, with registration and removal acting only on the selected ones (the three config files are unrelated; a user of just one CLI has no reason to get the other two written). Each row shows that CLI's config file path and registration state (not registered / N events registered / outdated N⁄M in yellow, prompting a re-register to pick up newly added events); the default selection is whichever CLIs are already registered (so an old user hitting register is a pure top-up), falling back to all three when none is, preserving the first-run one-click experience. Writes merge rather than overwrite your existing hooks. Codex permission requests stay in `ai-working` through approval and tool execution, avoiding premature completion notifications.
- **Real-time status detection** — Once hooks are reporting they are the status source for that pane; each polling round reads the hook state directly and never consults output activity (a TUI's idle redraws used to read as "working again," firing the completion notification over and over). Panes without hooks fall back to input detection (recognizing typed `claude` / `codex` / `opencode` / `pi` / `grok` commands, with a line-snapshot fallback for ↑ history and Tab completion) plus 500ms output-activity polling, showing idle / working / error states.
- **Grok Build hook integration** — `grok` (xAI's terminal agent) runs on the same hook pipeline as Claude and Codex: status badges, completion announcements, AI launchers, and mobile-initiated sessions all work. Three structural differences are each handled: (1) grok also scans `~/.claude/settings.json` for hooks by default, so the same event arrives twice — the sidecar identifies the compatibility-layer copy via `GROK_SESSION_ID` plus "was an argv passed" and drops it, while still letting it through when only Claude hooks are registered (then it's the sole source); the deciding factor is whether the native hook file is present. (2) The command registered into `~/.grok/hooks/` is a **bare filename with no spaces** (the hook binary is copied into that directory at registration), because a command containing spaces is handed to a shell — and on Windows which shell (git-bash / pwsh / powershell / cmd) depends on the environment, with mutually incompatible quoting; the event name travels via grok's injected `GROK_HOOK_EVENT` instead. (3) grok has no `PermissionRequest` event — "waiting for your approval" is a `Notification` of type `permission_prompt`, normalized onto the same amber lamp, while its `task_complete` is an FYI rather than a to-do and lights nothing. One more thing is smoothed over: grok fires an extra `Stop` at session teardown (`reason` of `channel_closed` / `shutdown`), which would otherwise announce a bogus "task complete" every time you quit grok.
- **Grok's session log shape** — Unlike the other two ("one file, one session"), a grok session is an **entire directory**: `{grok_home}/sessions/{URL-encoded cwd}/{session-id}/`, with the transcript in `updates.jsonl` (an ACP session-update stream) and metadata in `summary.json`. Project matching **decodes the directory name** rather than encoding the project path (the latter would mean replicating its encoding crate's escape set byte for byte; for over-long paths that degrade to a `{slug}-{hash}` form, it falls back to the `.cwd` file inside the directory). A single message is streamed to disk as arbitrarily many chunk lines, so chunks must be accumulated until a boundary (tool call, turn completion, the other party speaking) — otherwise one answer shatters into dozens of mirror entries. Usage comes from the `usage` payload on `turn_completed` (broken down per model; ACP's input count folds in cache reads and writes, split into disjoint buckets that reconcile with `totalTokens`). **Tool rankings are empty for grok** — the persisted ACP `tool_call` carries only a human-readable title, never the actual tool name, and substituting the title would pour natural-language labels into the ranking.
- **Agents identified by input detection alone** — `opencode` / `pi` have no hook integration and no parseable local session log: status badges, completion announcements, AI launchers, and mobile-initiated sessions all work, but the conversation mirror, the AI history panel, and usage stats stay empty for them. The mirror's heuristic binding is gated behind a whitelist (`mobile_mirror::agent_has_session_log`) — anything outside it returns an empty mirror rather than falling back to the newest session file of another agent in the same project and pasting someone else's conversation into that pane. Command matching is an exact basename match, so `pip` / `ping` / `pixi` / `pi.py` are never mistaken for `pi`.
- **Three fallbacks for a stuck badge** — `Stop` simply doesn't fire in several cases: a turn ending on an API error emits `StopFailure` instead (mapped to ai-idle, lighting the tray yellow so you know to resend), and a user interrupt via Esc / Ctrl+C emits nothing at all (settled from input detection, cause=`Interrupt`). Whatever those two miss is caught by a **stall check**: if the hook state sits at ai-working while both the state and the PTY output stay silent for 10 seconds, it converges — to `idle` when an exit was already triggered (Ctrl+D / double Ctrl+C / `/exit`, with no hook event since to prove otherwise), and to `ai-idle` otherwise. All three write their verdict into the hook state **once**, so they converge instead of oscillating, and none of them uses a `Stop` cause, so none is ever announced as a finished task (precisely why the memoryless version of this fallback was removed in v0.9.3). Panes awaiting user approval (Codex's `PermissionRequest`, for one) are exempt from the stall check, which would otherwise wipe out the tray's yellow light along with the badge.
- **Status aggregation** — Aggregated layer by layer from pane → tab → project, with priority `error > ai-working > ai-idle > idle`.
- **Completion notification trio** — Fires the moment an AI task goes working → idle *and* the cause is a `Stop` event (permission requests, notifications, and elicitations also land on `ai-idle` and are no longer misreported as completion; the hookless fallback path still keys off the falling edge alone):
  - A bottom-right toast desktop notification (only for inactive projects, deduplicated per project).
  - A DONE badge in the project list, cleared on click.
  - Taskbar flashing (Windows) / Dock bouncing (macOS), triggered only when the window is unfocused.
  - A notification sound (a default tone synthesized via the Web Audio API, with support for a custom audio file).
  - All notification toggles are independently configurable, managed together under "Settings → AI → Notifications" (hook registration lives on the sibling "Hook events" page).
- **Awaiting-confirmation alert** — When the AI stops to ask for tool permission, needs an MCP form filled in, or ends a turn on an API error (`PermissionRequest` / `Elicitation` / `StopFailure` — the same rule that lights the project row amber), it fires one more alert through the same channels as above. Independently toggled and on by default (Settings → AI → Notifications → Trigger; it fires far more often than completion, so anyone who only wants completion alerts must be able to turn it off). The trigger is the **rising edge** of the amber light rather than "this cause is an awaiting-type one": the backend deliberately exempts these events from deduplication (a second authorization request in the same turn must not be swallowed), so keying off the cause alone would alert several times for one pending request. While the amber light stays on there are no repeat alerts; typing into that terminal counts as handling it (clearing the light), so only the next request forms a new rising edge. The toast uses the warning color plus an exclamation mark to stay distinct from the green "finished" one, and sets no DONE badge (that marks completion).
- **Tray status light** — A persistent system-tray light for global AI status: yellow = awaiting confirmation, blue = working, green = unread completion, gray = quiet, rotating through coexisting states while the window is unfocused; the right-click tray menu lists **every project with an AI session** and its status (including ⚪ AI-idle ones, not just the busy ones; ordered awaiting > working > done > idle, entry cap configurable, idle entries never light the lamp) and picking a project jumps straight to its most urgent pane, while a left click summons the main window and jumps to the session that needs you next (the same landing logic as the title bar status light; a setting turns the jump off so it only summons the window — Linux offers the right-click menu only). Notification classification only treats permission / confirmation wording as "awaiting" — API errors and retry waits never light yellow. Can be disabled in Settings.
- **Automatic session resume** — After a restart, each split pane automatically writes `claude --resume` / `codex resume` / `grok --resume` to reconnect its previous session: session identity comes from hook reports and persists with the layout across one restart; everything written back is allowlist-checked (alphanumerics plus `-_` only, max length 128), remote panes are excluded, and anything unrecognizable is never written. Can be turned off under Settings → System → General (terminals still come back, they just don't run the resume command).
- **Session enter/exit detection** — Recognizes entering AI via command echo; recognizes exit via a double `Ctrl+C` / `Ctrl+D` or `exit` / `quit` / `:quit` / `/logout`.
- **Session history** — Reads local Claude / Codex / Grok history records, with a right-click to copy the resume command for quick continuation; the first screen renders only 20 entries, with a "Load more" button at the bottom to expand on demand (no longer triggered by scrolling).
- **Session viewer** — A right-click "View" shows the full conversation, with User as plain text and Assistant rendered as Markdown (external links open in the system default browser after a confirmation prompt), supporting `Ctrl+F` search highlighting and quick navigation between User messages.
- **WSL sessions** — Reads Claude / Codex session history inside WSL distros directly from Windows (no `wsl.exe` spawn — via `\\wsl$` UNC plus registry-based distro enumeration): WSL-rooted projects auto-derive the distro and path with zero configuration; Windows-path projects pick a distro via the right-click "WSL Sessions" submenu and are scanned through `/mnt` path mapping, with in-session cwd verification to prevent cross-project mixing; WSL sessions merge chronologically with local ones under a WSL badge, a header spinner shows while loading, and viewing session content is supported too.
- **AI task markers** — Each time the user presses Enter inside an AI session, a marker is dropped in xterm; the ⚑ button at the tab's top-right drops down the list of past submissions, and clicking one or pressing `Ctrl+Shift+↑/↓` (macOS `⌘+Shift+↑/↓`) jumps between markers, briefly highlighting the target line.

### Usage Statistics

- **Multi-dimensional panel** — The "Stats" button in the top bar opens a panel aggregating Claude Code / Codex / Grok cost, call count, and session count as KPI groups, with daily / hourly trend charts (recharts), model rankings, project rankings, and top sessions; agent / time-range / project filters are one click away.
- **rusqlite local ledger** — Local session JSONL files are parsed into a SQLite ledger; panel queries return in milliseconds while incremental sync catches up in the background (files are re-parsed only when their fingerprint changes). The ledger is positioned as "a cache regenerable from the raw records": corruption triggers an automatic rebuild, and there is no migration burden.
- **Billing accuracy** — History duplicated by session forks is deduplicated by lineage and never double-billed; cache writes / reads are priced precisely at the official rate differentials (1h cache writes at 2× input price, 1h subsets pay only the difference); unknown models are estimated at the average of Claude's mainline tiers.
- **Price table** — Fetched once a day from models.dev (a read-only GET of a public price list — **no usage data is ever uploaded**); on failure the local cache is used, and the panel never shows made-up numbers.

### Mobile Client + Self-Hosted Relay

Watch the AI running on your desktop from your phone while you're out, and send it commands directly.

**Prerequisite**: you need your own publicly reachable server to run the relay (1 vCPU / 1 GB is plenty, one Docker command to start, plus a domain pointed at it for TLS — see the [deployment guide](deploy-relay.md)).

- **Connect and pair in one place** — Fill in the relay address in the top-bar "Mobile" panel → save & connect → generate a pairing QR code, all in a single panel. Scanning with your phone camera opens the PWA and pairs automatically; the code is single-use (valid for 10 minutes), pairing a new device replaces the old one, and "Reset pairing" revokes every credential instantly.
- **Active AI session list** — The phone shows running Claude / Codex / Grok sessions grouped by project, with status lights that add, remove, and change color in real time alongside the desktop; when the desktop goes offline a top banner appears and the list greys out, clearing automatically on reconnect.
- **Start a new session from your phone** — Tap **+** → pick a project → pick an AI launcher, and the desktop opens a terminal tab in that project in the background and brings the agent up; once the session is really running the phone enters its mirror automatically, without disturbing whatever you are looking at on the desktop. Projects are listed with the desktop's group hierarchy and can be collapsed. Launchers are named entries configured on the desktop — the phone references them by id and only ever sees the name, so **the command text never passes through the phone or the relay**.
- **Rename sessions** — Give a session a name you will recognise, from the ✎ on a list row or the title on the mirror page; it shows up on the desktop terminal tab as well. Leave it empty to restore the default name.
- **Conversation mirror (read-only)** — Tap into any session to follow the conversation live, with AI replies rendered as Markdown and desktop input shown verbatim; scrolling to the top pages in older messages. Mirror binding resolves the session identity through hooks down to the exact pane, so multiple AI sessions running in the same project never cross-contaminate.
- **Mobile commands** — The input box at the bottom of the mirror page writes text straight through to the corresponding desktop terminal (equivalent to typing it yourself and pressing Enter), with an immediate receipt and an explicit failure reason; when the desktop is offline the relay rejects the command outright rather than storing and forwarding it.
- **The relay forwards, never persists** — The relay server stores no message bodies and logs metadata only (a subprocess-level automated test asserts zero file residue across the full flow); it ships with a three-stage Dockerfile and a compose example to build and run from source in one command — reverse proxy + TLS setup in the [deployment guide](deploy-relay.md).
- **PWA experience** — "Add to Home Screen" runs it as a standalone window, with exponential-backoff reconnection that automatically restores subscriptions, and the same bilingual (English / 中文) layer as the desktop app.

### Project Management

- **Project list** — Manage multiple project directories in the left sidebar, switch workspaces in one click, and restore the last active project on restart.
- **Project descriptions** — Right-click "Edit description" to add a one-line note, shown in gray after the project name; tell a row of worktree sub-projects apart at a glance.
- **Project row icons** — Project rows show tech-stack icons and the brand icons of the AIs currently running there (deduplicated by vendor, alphabetical, monochrome brand icons tinted in brand colors); pane tabs and the session list show the same brand icons.
- **Hover pane preview** — **Only for projects running an AI session** (same test as the row's AI brand icons, `paneShowsAiSession`: if the icon is lit, the preview exists; the overlay closes as soon as the AI exits, and hovering a plain shell project just shows the absolute path as a tooltip instead of interrupting you with a card). Hover such a project row for 250ms and a **miniature layout puzzle** of its terminal area pops up: split proportions reproduced from the SplitNode tree's flex-grow, in a fixed-width overlay that never runs off screen, matching what you'd see after switching; it redraws every 500ms while open, so the preview is live. It reads the buffer and draws it rather than taking a screenshot — a hidden pane's xterm instance has rendering paused and WebGL has no `preserveDrawingBuffer`, so its pixels aren't trustworthy, but its buffer keeps being updated by the global `pty-output` listener. Same-style runs are extracted from the viewport through a structural interface (wide chars get their own run, bold standard colors brighten, 256-color / truecolor resolved), painted onto an 8px cell grid and scaled proportionally. Each split leaf shows its active tab (cover + bottom-left anchored, preserving the newest output and the TUI input area); hidden tabs are summarized by a "+N" badge carrying the highest-priority status among them (error > ai-working > ai-idle, same ordering as the store aggregate), so AI activity buried in an inactive tab isn't missed. Panes without a PTY show a "Not started" placeholder; the absolute path stays visible in the card header, and falls back to the row's `title` when there is no overlay. **Inactive pane tabs** also pop a single-cell thumbnail overlay after a 250ms hover (`PaneTabPreview.tsx`, same rendering pipeline: cached terminal buffer → run extraction → canvas bitmap, redrawn every 500ms while open; the "Not started" placeholder and remote-disconnect veil follow the MiniPane conventions), with **no AI gate** — a hidden tab's content is invisible until you switch to it anyway, and the preview answers exactly "what's on that tab right now". The trigger timing matches the project-row preview (250ms timer, rect taken when it fires, `isConnected` liveness check); it closes on mouse-out / click / context menu / scroll, and teardown uses the "render gate + effect clearing state" double latch (closing the tab via its X is stopped by stopPropagation, so a render gate alone would leave a stale anchor behind); the card clamps to the horizontal edges and flips above the tab when a bottom split leaves no room below.
- **Drag to add projects** — Drag a folder from the file explorer onto the project list to add it quickly, with automatic detection of files / folders / duplicate projects and visual feedback.
- **Nested groups** — Up to 3 levels of project grouping, drag to reorder, collapse / expand, with a group context menu to add either a local project or a remote SSH project directly into that group (a collapsed group expands automatically). "Delete group" now asks for confirmation first, explaining that the projects inside move up one level rather than being deleted; "Move to group" expands the group tree level by level as submenus, marking the current group with a ✓ and greying it out, with over-depth groups unselectable.
- **Worktree sub-projects** — A worktree turned into a project is mounted beneath its main project as a sub-project (indented, following the group), and can be dragged out or detached via "Detach from parent" to return to the top level; deleting a parent project promotes its sub-projects in place instead of losing them. The project list shows a ⎇ branch badge for worktree projects, and the repo list and Changes dropdown label worktree entries as well. **Externally removed worktrees are reconciled automatically** — whenever the window regains focus, sub-project directories are probed for existence, so after an AI agent runs `git worktree remove` in a terminal the vanished sub-project is dropped along with its terminal resources and the ⎇ badges are re-probed (cleanup only happens while the parent project directory still exists, so a disconnected drive can't wipe entries; SSH remote and UNC/WSL paths are excluded). "Clean up stale entries" in the worktree modal removes the projects pointing at those worktrees too.
- **File tree** — An integrated directory browser with natural sorting (V1 → V2 → V10 rather than lexicographic), nested `.gitignore` greying (ignore rules and `!pattern` allowlists at every sub-directory level take effect, consistent with git behavior), and live refresh via `notify` file watching.
- **File operations** — Create / rename / delete files and folders and view contents inside the file tree (Markdown rendering supports HTML tags and external images, external links open in the system default browser after a confirmation prompt, image formats are shown directly, HTML files preview in an iframe with relative-path resources auto-resolved, and binary / oversized files get a friendly notice).
- **Built-in file editor** — Click any file in the tree to edit it in place (CodeMirror 6 core): syntax highlighting for 140+ languages matched by file type and lazy-loaded on demand, find & replace (`Ctrl+F`), code folding, bracket matching, and multi-cursor editing; `Ctrl+S` saves atomically (temp file + rename), CRLF files round-trip with their original line endings so you never get a whole-file diff; closing or navigating away with unsaved changes asks first, and external modifications reload silently when clean or show a notice bar when dirty; Markdown / HTML previews render the unsaved draft live; syntax colors reference the app palette via `--syn-*` variables and follow all four theme skins.
- **Open in external editor** — A button at the top-right of the file tree opens the current project in your configured editor (VS Code by default), with the path customizable under "Settings → System → Editors"; files can be opened with the system default app.
- **Project-level environment variables** — The project context menu "Environment Variables…" opens a management dialog with a row-level `[enable checkbox][key][value][✕]` layout, injecting per-project variables into the PTY child process when starting that project's terminal; strict POSIX validation (key matches `^[A-Za-z_][A-Za-z0-9_]*$`, no `MINITERM_` prefix, no `WSLENV`, no duplicates within a project, and value forbids `\n/\r/\0`); the Rust side adds a defensive `MINITERM_`-prefix + `WSLENV` filter, so even hand-editing `config.json` to bypass frontend validation cannot break the hook protocol or WSLENV concatenation; under WSL projects, variables pass through to Linux bash via the WSLENV mechanism (`/u` is one-way without path translation; an `export` of the same name in `~/.bashrc` will override).

### Git Integration

- **File status** — The file tree shows Git status colors (modified / added / deleted / conflict).
- **Changes / history in one view** — The Git panel stacks two collapsible sections: Changes on top and commit history below, with a draggable divider (clamped 15%–85%) and animated collapse / expand, remembering fold state and ratio for the session; a repo bar at the top of the panel switches repos via a dropdown (worktree entries marked ⎇), clicking the branch badge only switches which branch's history is shown (no checkout, highlighted when viewing a non-HEAD branch), refresh / Pull / Push sit on the same bar, and right-clicking the repo name opens it in a terminal or enters worktree management.
- **Change diff** — A detailed diff of working-tree file changes, parsed at the hunk/line level, with side-by-side / inline dual views; side-by-side mode supports dragging to adjust the split ratio, and the font size follows the terminal font setting.
- **Commit history** — A flat list of the commit log for the repo selected in the top repo bar, with cursor-based pagination (30 entries by default).
- **Branch topology graph** — Each history row draws an SVG topology graph on the left, laying out branch, merge, and pass-through lines by lane, coloring nodes per lane and marking merge commits with a filled dot inside an outer ring; merge-in lines use the branch's own color as a Bézier curve that gradient-blends into the mainline at its root. The backend revwalk appends TOPOLOGICAL sorting so clock skew or a rebase can't place a parent after its child and break the lines, and a commit row is only labeled with the branches this repo itself has checked out, rather than hanging every other worktree / remote branch on it.
- **Commit diff** — View the file changes of any commit, switching file by file.
- **Branch info** — Local / remote branch lists.
- **Source control panel** — A VS Code-style Changes panel grouping Staged / Changes / Untracked, supporting per-file and bulk stage / unstage / discard, `Ctrl+Enter` to commit quickly, and toggling between list and tree views.
- **Pull / Push** — Buttons on the top repo bar sync with the remote in one click, with a refresh button to reload the commit log and branch info.
- **Multi-repo discovery** — Automatically scans all Git repos under the project directory (recursing 5 levels, skipping `node_modules` etc.).
- **Worktree management** — Right-click a project or the repo bar at the top of the Git panel to open the "Worktree management" dialog: list every worktree, create one from an existing branch or a new branch, remove it (force optional), and prune stale entries, with the repo list refreshing immediately after any change; a worktree can be turned into a project in one click or opened directly in a terminal, and panes support a working-directory override that persists with the layout and is inherited by splits. When the project root itself isn't a repo, it scans downward for sub-repos and groups them by main worktree into a list whose group headers are checkable (multi-select / select-all), creating one worktree per checked repo in a single action — the branch dropdown then offers the intersection of all repos' branches, the path field becomes a parent directory previewing the `<repo>-<branch>` landing spot, and failures are listed per repo.

![Git integration](screenshots/git.png)

### Appearance & Configuration

- **Icon sidebar + three-column layout** — A persistent icon bar on the far left (collapse middle column / Sessions / Git / Settings / SSH); the middle column stacks Projects over Files and collapses as a whole; the terminal sits on the right. Sessions / Git are now floating drawers that slide out from the right edge over the terminal (mutually exclusive single panel, left-edge drag to resize with persisted width, ✕ to close), with a blue vertical bar indicating the active state.
- **Three theme modes** — Auto (follows the system) / Light / Dark, with Dark based on a Warm Carbon palette and a custom CSS-variable system; the title bar is drawn by the app and reads the theme variables directly, with no first-frame light flash for dark-mode users on startup (the Windows window border still syncs via DWM Immersive Dark Mode).
- **Custom title bar** — System decorations are dropped (`decorations: false`) in favour of a self-drawn 32px bar: app name, version, project switcher and global status light on the left, window controls on the right, coloured from the theme instead of the system's grey strip. Adapted to each platform's conventions:
  - **Windows / Linux** — Minimize / maximize / close on the right, with the close button turning red on hover. Win11 **Snap Layouts** still work: `window_snap.rs` subclasses the window procedure and returns `HTMAXBUTTON` for the maximize button's rectangle in `WM_NCHITTEST`, so hovering pops the snap menu. That rectangle thereby becomes non-client area and stops receiving WebView events, so hover highlighting is relayed back to the frontend via a `titlebar-max-hover` event and clicks post `WM_SYSCOMMAND` directly.
  - **macOS** — The native traffic lights are kept (`titleBarStyle: Overlay` + `hiddenTitle`) with space reserved in the top-left corner; no hand-drawn dots, so full-screen, gestures, and system integration all survive.
  - **Project switcher** — A pill button next to the version number, set off by a divider, always showing the current project's name with its own AI status dot (dimmed when it has no AI session). Its dropdown lists every project with an AI session and its status (the same aggregation the tray menu uses, ordered awaiting > working > done > idle); clicking a project switches to it and lands on its most urgent pane, or just switches when everything is quiet.
  - **Global status light** — Sits right beside the project switcher and aggregates the most urgent state across every pane of every project (error > awaiting confirmation > working > done). Clicking jumps to the session that needs you next: awaiting-confirmation / errored first, then the **earliest finished** one, and only then anything still running. This deliberately differs from the tray context menu's ordering — the tray answers "which projects are still alive", the status light answers "what should I do next".
  - Dragging goes through Tauri's `startDragging` rather than `-webkit-app-region`, avoiding the WebView2 modal-loop input lockup fixed back in v0.2.16; double-clicking the bar toggles maximize.
- **Blueprint skin** — An optional sci-fi Blueprint skin with a grid background + corner markers + glow effects, supporting both dark and light modes, with the terminal palette switching in sync.
- **External theme packs (Dream Skin-compatible)** — Settings → Appearance → Theme & language can import third-party skins from a folder or a zip into `{app_data_dir}/themes/<themeId>/` (`theme.json` required; `theme.css` / background image optional). "Create example" in the same section writes a ready-to-edit sample skin into `themes/example/` (`theme.json` + `theme.css` + a `README.md` documenting every field; saving hot-reloads it). The sample is literally the **same file** as [`docs/theme-pack-example/`](theme-pack-example/) in the repo (embedded at compile time via `include_str!`, so docs and product can't drift apart), and it errors instead of overwriting when the folder already exists — a copy you've edited is never silently wiped. When the pack ships a `manifest.json`, every file is checked against its bytes + sha256 to catch corruption; imports land in a staging directory first and are swapped in atomically only after validation, so a bad pack can't take out an existing skin of the same name. A pack's light/dark nature is fixed by its author via `appearance` in `theme.json`, and the built-in theme buttons show as unselected while it's active. Editing a file in the pack hot-reloads it (300ms debounce). A pack may declare a background image, in which case terminal surfaces turn translucent over that ambient layer and WebGL rendering falls back to DOM (upstream requires an opaque canvas), restored automatically when you switch back to an opaque theme — the DOM path quantizes cell width to whole device pixels to match WebGL and the terminal font stack carries explicit CJK fallbacks, so letter spacing and full-width punctuation stay consistent with built-in themes; any `terminal.background` the author wrote is dropped in that mode — it expands after the transparency step, so a pack that copied all 24 built-in fields would otherwise bury the ambient image entirely. An imported `theme.css` goes through a hygiene check: 256KB cap, no `@import`, and any reference pointing outside the pack rejected — the check runs on a sample with comments stripped and CSS escapes resolved, and inspects both `url()` and bare string literals (Chromium honors `image-set("https://…" 1x)`, which fires a request without any `url()`), so forms like `url(\68 ttps://…)` are caught too. The `tokens` escape hatch in `theme.json` is held to the same standard: keys must be `--` custom properties and values pass the same gate — without the `--` prefix `setProperty` sets a **real CSS property**, and one line of `{"background-image":"url(https://…)"}` would bypass every check above (the csp in `tauri.conf.json` is null, making this the only gate).
- **Independent font tuning** — The UI and terminal font sizes (10-20px) / families are adjustable separately, and the terminal can optionally follow the UI theme.
- **Ligatures** — A terminal ligature toggle that composes glyphs like `==` `=>` `!=` `->` when enabled, requiring a font with a calt table (Fira Code / JetBrains Mono); fully supported on Windows, while macOS / Linux use a 60-entry Iosevka fallback due to webview API limitations.
- **Layout persistence** — Split ratios, tabs, and window size / position are saved automatically and restored on restart (`tauri-plugin-window-state`).
- **Close confirmation** — Closing the window takes stock of AI sessions only (panes in ai-working / ai-idle); plain shell terminals no longer count, and the confirmation appears only when AI sessions exist, listing their names. All project layouts are flushed either way.
- **Update check** — Fetches the GitHub Release on startup; when a new version is available a highlighted hint appears on the icon sidebar (click to download), and the version number is written into the native window title.
- **Bilingual UI (English / 中文)** — A one-click language toggle under "Settings → Appearance → Theme & language" instantly re-renders the entire interface; the language is auto-detected from the system on first launch and remembered across restarts. Every page and feature is fully translated, with a lightweight built-in i18n layer (no extra runtime dependency).
- **Settings center** — A unified SettingsModal whose sidebar is a two-level "group + page" menu: Terminal (Shell / Copy & paste), Appearance (Theme & language / Font), AI (Notifications / Hook events), System (General / Editors), with Shortcuts and About kept at the top level. Grouping by topic keeps every page to roughly one screen, ending the old "nine control groups on one page, scroll half a page to find a toggle" problem; page ids are unchanged, so external deep links (`initialPage`) survive the reshuffle.
- **Icons everywhere** — Material-theme file / folder icons in the file tree (including open-folder states); the full icon dataset (gzip ≈1.2MB) is a separate dynamically-imported chunk with zero main-bundle growth, falling back to the original hand-drawn symbols until loaded; AI brand icons are imported as pure SVG components via deep paths.
- **Startup performance** — Fonts are bundled locally (@fontsource woff2 shipped with the installer, removing the render-blocking Google Fonts link), so the startup path makes zero network requests and the offline first frame no longer waits on fonts; five heavy modals (Settings / File viewer / Session viewer / Mobile / Stats) are React.lazy-loaded on demand, bringing the main bundle from 631KB to 378KB gzipped; a unified Rust / WebView startup-timeline trace is written to stderr for regression hunting.
- **Interface motion** — Dialogs, context menus, and the side drawer share one enter/exit animation: the backdrop fades in while the panel drops and scales into place; on close it plays the reverse before unmounting (content is frozen and the overlay leaves the stack meanwhile, so it never goes blank mid-fade or keeps swallowing Esc). Context menus expand from the cursor, and switching terminals or creating a split each get their own transition. When the system disables window animations (`prefers-reduced-motion: reduce`) these transitions still play — the usage panel's number tweens and chart animations are exempted likewise — only looping animations such as the blinking status dot are stopped.

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
| Usage stats | rusqlite 0.40 local ledger · recharts 3 trend charts · chrono-tz timezone bucketing |
| Tauri plugins | `window-state` · `clipboard-manager` · `dialog` · `opener` |
| Mobile relay | axum + tokio WebSocket relay service (`relay-server/`) · React + TS + Vite PWA (`mobile/`) |
| Test coverage | 618 Rust tests = 565 desktop (tauri-app 411 + mt-core 44 + mt-ssh 26 + mt-sidecars 84) + 53 relay-server (protocol & routing); plus 100 Node tests |

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
- [Rust](https://www.rust-lang.org/tools/install) >= 1.95 — set by libsqlite3-sys 0.38's use of `cfg_select!`, stable since 1.95 (Tauri v2 itself only needs 1.77.2)
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
│   │   ├── SessionList.tsx       # AI session history list (Claude / Codex / Grok)
│   │   ├── FileTree.tsx          # File directory tree + Git status + create / rename
│   │   ├── TerminalArea.tsx      # Tab management + split-tree operations
│   │   ├── SplitLayout.tsx       # Recursively renders the SplitNode tree
│   │   ├── TerminalInstance.tsx  # xterm.js instance + context menu + file drop
│   │   ├── PaneGroup.tsx         # Split group container
│   │   ├── MarkerList.tsx        # AI task marker dropdown
│   │   ├── GitHistory.tsx        # Git panel container: repo bar + collapsible Changes/History sections + Pull / Push
│   │   ├── GitHistoryContent.tsx # Selected repo's commit history list rendering
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
│   │   ├── SettingsModal.tsx     # Settings dialog (two-level menu: terminal / appearance / AI / system + shortcuts / about)
│   │   ├── LanguageToggle.tsx    # Chinese / English switcher
│   │   ├── ToastContainer.tsx    # AI completion / awaiting-confirmation toasts
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
│       ├── builtinThemes.ts      # Single source for the 6 built-in terminal palettes
│       ├── themePackManager.ts   # External skins: validation / token mapping / background layer / theme.css gate / hot reload
│       ├── panePreview.ts        # Pane preview: xterm buffer → same-color run grid (pure logic, directly testable)
│       ├── panePreviewCanvas.ts  # Pane preview: run grid → canvas bitmap (8px cells, scaled proportionally)
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
│   │   ├── ai_sessions.rs        # Claude / Codex / Grok session record reading (local + WSL UNC; Grok local only)
│   │   ├── wsl_distros.rs        # WSL distro enumeration (registry Lxss, no wsl.exe spawn)
│   │   ├── theme_packs.rs        # External skin scanning / folder & zip import / manifest sha256 verification
│   │   ├── hook_server.rs        # Hook HTTP server (receives AI tool events)
│   │   ├── hook_registry.rs      # Hook register / unregister (Claude Code + Codex + Grok, per-CLI selection)
│   │   ├── ssh.rs                # SSH connection management + password auto-fill / key handling
│   │   ├── remote_ssh.rs         # SSH remote projects (SFTP dir listing / dir validation / remote session reading)
│   │   ├── ssh_skill_registry.rs # Per-project SSH tools enablement (generates Claude / Codex SKILL.md)
│   │   ├── ssh_mcp_registry.rs   # Legacy MCP registration cleanup (migration fallback for old projects)
│   │   ├── mobile_relay.rs       # Mobile relay (outbound WSS link / pairing / snapshots / commands / start session / rename)
│   │   ├── mobile_mirror.rs      # Conversation mirror (incremental session JSONL parsing + pagination)
│   │   ├── window_theme.rs       # Windows window-border dark mode (DWM Immersive Dark Mode)
│   │   ├── window_snap.rs        # Win11 Snap Layouts (HTMAXBUTTON hit-testing for the frameless window)
│   │   └── window_input_recovery.rs # Recovery from stuck window input focus
│   ├── mt-core/                  # Shared library crate without tauri deps (SSH types / config / keys)
│   ├── mt-ssh/                   # Shared SSH crate (persistent russh session pool + SFTP primitives, used by both the app and sidecars)
│   └── mt-sidecars/src/bin/      # Standalone sidecar crate (no tauri-build dependency)
│       ├── miniterm-hook.rs      # Hook CLI tool (called by AI tool hooks)
│       ├── mt-ssh-cli.rs         # SSH CLI (called by terminal AI agents via Bash; daemon-backed pool)
│       └── mt-ssh-mcp.rs         # SSH MCP server (rmcp stdio; transition-period legacy channel)
├── relay-server/                 # Self-hosted relay service (standalone Rust workspace)
│   ├── protocol/                 # Protocol message crate shared by desktop and relay (JSON over WebSocket)
│   ├── server/                   # axum relay service (forward-only, no persistence + PWA static hosting)
│   └── docker-compose.yml        # Build and run from source in one command
├── mobile/                       # Mobile PWA (React + TS + Vite — pairing / list / mirror / commands / start / rename)
├── scripts/
│   ├── stage-sidecars.mjs        # Builds sidecars and stages them per-triple as Tauri externalBin
│   └── stage-conpty.mjs          # Downloads, verifies and stages the pinned ConPTY runtime (Windows)
├── tests/                        # Node-side tests (20 files, 100 cases: ConPTY bundling / TUI scrollback / layout restore / theme compat / WSL path / worktree reconcile ...)
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
ai-working → ai-idle(Stop)          → Toast + DONE Tag + requestUserAttention
attention rising edge(PermissionRequest…) → Toast(warning) + sound + requestUserAttention
```

### Tauri Interface Overview

- **Commands (70)** — PTY: `create_pty` · `write_pty` · `resize_pty` · `kill_pty`; FS: `list_directory` · `read_file_content` · `watch_directory` · `unwatch_directory` · `create_file` · `create_directory` · `rename_entry` · `delete_entry` · `filter_directories`; Search: `start_search` · `cancel_search`; Git: `get_git_status` · `get_git_diff` · `discover_git_repos` · `get_git_log` · `get_repo_branches` · `get_commit_files` · `get_commit_file_diff` · `git_pull` · `git_push` · `get_changes_status` · `git_stage` · `git_unstage` · `git_stage_all` · `git_unstage_all` · `git_commit` · `git_discard_file` · `list_worktrees` · `add_worktree` · `remove_worktree` · `prune_worktrees` · `get_worktree_branches`; Config: `load_config` · `save_config`; Editor: `open_in_editor` · `open_path_with_default_app`; Clipboard: `read_clipboard_image` · `save_clipboard_text`; AI: `get_ai_sessions` · `get_wsl_ai_sessions` · `get_ai_session_content`; WSL: `list_wsl_distros`; Hook: `register_ai_hooks` · `unregister_ai_hooks` · `get_ai_hook_registrations` · `get_hook_config_snippet` · `get_hook_status` · `toggle_hook_server`; SSH: `arm_ssh_autofill` · `prepare_ssh_key`; SSH tools: `enable_ssh_tools` · `disable_ssh_tools`; SSH remote: `ssh_remote_list_directory` · `ssh_remote_validate_dir` · `ssh_remote_ai_sessions` · `ssh_remote_ai_session_content` · `ssh_remote_upload_paste`; Theme: `set_window_dark_mode`; Mobile relay: `mobile_relay_apply` · `mobile_relay_status` · `mobile_relay_request_pairing_code` · `mobile_relay_reset_pairing` · `mobile_relay_update_sessions` · `mobile_relay_launchers_changed` · `mobile_relay_start_session_result` · `mobile_relay_check_launcher_command`
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

# Node-side tests (20 files, 100 cases)
node --test "tests/*.test.cjs"

# Desktop Rust tests (565)
# Note: mt-core / mt-ssh / mt-sidecars are standalone crates, not workspace members.
# Running `cd src-tauri && cargo test` alone only covers tauri-app's 411 — the other
# three need their manifests specified explicitly.
cd src-tauri
cargo test                                        # tauri-app     411
cargo test --manifest-path mt-core/Cargo.toml     # mt-core        44
cargo test --manifest-path mt-ssh/Cargo.toml      # mt-ssh         26
cargo test --manifest-path mt-sidecars/Cargo.toml # mt-sidecars    84
cargo build

# Relay server tests (53, standalone workspace)
cd ../relay-server && cargo test
```

## Community

Learn AI, join the L site — [LinuxDO](https://linux.do/)

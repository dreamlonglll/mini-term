<p align="center">
  <img src="icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>A desktop terminal manager built for the AI era</strong><br>
  GPUI-native · Multi-project · Multi-tab · Split-pane layout · AI process awareness · SSH remote projects · Git worktree management · Watch your AI from your phone
</p>

<p align="center">
  <a href="features.zh-CN.md">简体中文</a> · <strong>Full feature list · English</strong><br>
  <a href="../README.en.md">← Back to the project home</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-1.1.7-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/GPUI-native-8A2BE2" alt="gpui">
  <img src="https://img.shields.io/badge/Rust-1.95%2B-dea584" alt="rust">
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
- **Recursive splitting** — Arbitrarily nested horizontal / vertical splits, drag to adjust ratios.
- **Pane drag to rearrange & merge** — Terminal tabs can be dragged as a unit: drop onto another group's tab bar or the center of its terminal area to merge into that group (the tab bar shows an insertion indicator, and dragging within the same bar reorders tabs), or drop onto one of the four edges (25% depth) to split off a new pane in that direction, with a translucent drop preview. Dragging uses GPUI's built-in drag & drop (on_drag / on_drop), Esc cancels mid-drag, and cached terminal instances migrate intact through the layout-tree rearrangement — terminal content and PTYs are unaffected.
- **Pane maximize** — Double-click the tab bar's empty area (or the maximize button on the right) to temporarily fill the terminal area with the current group; double-click or press again to restore. The state is runtime-only (never persisted), closing the maximized pane falls back to the full tree view automatically, and splitting while maximized restores first so the new pane is never invisible.
- **High-performance rendering** — alacritty_terminal parses VT in-process with GPU-native rendering — zero IPC, zero serialization; minimum contrast is enforced, fixing Claude's prompt text being nearly invisible against a dark background.
- **Configurable scrollback buffer** — The number of retained normal-buffer lines is adjustable in Settings (10,000 by default; lowering it takes effect immediately and frees the memory — an early version hard-coded 100,000 lines and could be pushed to out-of-memory across enough projects and splits, a lesson baked into today's default). Standard CSI 3J (ED3) is honored globally, so applications such as Codex can discard transient output and replay a folded transcript, while `/clear` can truly purge old history. On Windows, mini-term bundles and preloads a pinned official ConPTY compatibility runtime (with a system-ConPTY fallback if validation fails) to keep Codex scrolling and transcript folding consistent across Windows versions.
- **Terminal caching** — Switching projects / tabs / panes never rebuilds the terminal instance, so existing content is preserved; lazy startup creates a PTY only for the currently visible pane, avoiding the slowdown of spawning more terminals the more history projects you have.
- **Project-switch caching** — File-tree / Git-history data is cached per project, so switching back to a visited project renders with zero latency; directory loading and Git status run in parallel.
- **Copy & paste** — `Ctrl+Shift+C/V` (macOS `⌘+Shift+C/V`) shortcuts + context menu, with "Copy" auto-greyed when nothing is selected; an optional "Smart `Ctrl+C/V`" mode (copy when there's a selection, interrupt the program when there isn't, and `Ctrl+V` pastes directly; on macOS the `⌘` combos are not governed by that switch); on Windows, large multi-line pastes are chunked to prevent ConPTY from dropping lines.
- **Dwell-to-copy selection** — After drag-selecting, holding the mouse still past a configurable dwell (default 1s, 0.2–60s, 0 = off) copies the selection and shows a "Copied" tip at the cursor; if the selection kept growing before mouse-up, it copies once more so the clipboard always matches the final selection.
- **Alt / ⌥+click to place the cursor** — Hold Alt (⌥ on macOS) and click a cell in the terminal: left/right arrow keys are synthesized from the column delta to the current cursor (following DECCKM application-cursor mode; zero delta sends nothing, more than 512 steps gives up, and it is inactive while scrolled back). **Same line only** — cross-line clicks never move: in a line editor the vertical arrows usually recall history rather than move the cursor, and not moving beats destroying what you were typing. Cell-accurate at bash / zsh / pwsh prompts; Ink-style TUIs such as Claude CLI park the hardware cursor at the end of the input line, so the starting point doesn't line up and accuracy is best-effort.
- **Long-text paste** — When clipboard text is ≥10 lines or ≥2000 chars, it is automatically saved to a temporary `.txt` and a quoted file path is pasted instead, avoiding the performance and paste-bracket issues of feeding huge content straight to AI tools.
- **Image paste** — Detects screenshots in the clipboard. On Windows it reads the Win32 clipboard (`CF_DIB` / `CF_BITMAP`), saves a temporary PNG and pastes a quoted path — compatible with non-standard formats such as PinPix; other platforms write the system clipboard's raw PNG/JPEG bytes straight to disk. When an image is present but cannot be decoded (e.g. a `BI_BITFIELDS` bitmap), it sends `Alt+V` so the AI tool running in the terminal can read the clipboard itself.
- **Remote / WSL paste lands where the agent can read it** — Both "save to a file, paste the path" features above automatically remap their destination in remote terminals: SSH remote projects upload the file over SFTP and paste the **remote** path (default `<project root>/.mini-term/pasted`, inside the project so agents need no extra permission; configurable to `/tmp/mini-term`, `~/uploads`, etc., and a self-ignoring `.gitignore` is written so your `git status` stays clean), while WSL projects rewrite `C:\...` into `/mnt/c/...` (no upload needed). Upload failures raise an explicit toast instead of pasting a local path the remote host cannot read.
- **File drag & drop** — Dragging a file from the file tree or system file explorer onto the terminal inserts its quoted absolute path, targeting the exact split pane and handling paths with spaces. Press `Esc` mid-drag to cancel on the spot: no path is written to the PTY (the Esc is swallowed by the drag layer, so it never reaches the terminal as `\x1b`), releasing the mouse doesn't degrade into a plain click that opens the file, and the hover indicator is cleared along with it. Esc is only swallowed once the drag is actually active, so Esc elsewhere still behaves normally.
- **Multiple shell profiles** — Windows (cmd / powershell / pwsh), macOS (zsh / bash), Linux (bash / sh) and more, freely added or removed.

### SSH Connections

- **Connection management** — The top-bar "SSH" button opens a management dialog with a two-pane layout (group list on the left, connections of the selected group on the right) to add / edit / delete SSH connections, with host / port / username / password / private key / group fields, persisted to the config file. The "Link SSH" and "Add remote project" dialogs share the same structure (one group-bucketing implementation, collapsible groups in the All view, and select-all / clear-all acting only on currently visible connections), and deleting a connection asks for confirmation first, warning that the stored password and private-key path will be lost.
- **Quick connect** — A right-click "SSH Connect" submenu inside the terminal lists saved connections by group; selecting one assembles the `ssh` command and launches the session right in the current terminal.
- **Password auto-fill** — For connections with a saved password, the backend scans PTY output for the password prompt and writes the password back automatically, once per session, stopping on a wrong password to avoid hammering the server with bad credentials.
- **Private-key permission handling** — When connecting with a private key, the key is copied to a permission-tightened temporary copy (Windows `icacls` / Unix `0600`) to bypass OpenSSH's "UNPROTECTED PRIVATE KEY FILE" rejection, without modifying your original key file.
- **Advanced capabilities** — Key-file login (`ssh -i`) and connection grouping: right-click to create / rename / dissolve groups (empty groups persist), drag a connection onto a group to move it, and pick existing groups from a dropdown in the edit form.
- **SSH tools for AI agents (CLI + Skill)** — Lets AI agents running in the terminal (Claude Code / Codex) operate on saved SSH connections. The project right-click "Link SSH" menu enables them per project and limits visibility to the selected connections; enabling generates two SKILL.md files (Claude / Codex variants, with the CLI's absolute path and a random per-project capability token baked in, `.gitignore` entries appended, and any legacy MCP registration migrated away automatically). The token is required on every `list` / `exec` / `upload` / `download` call; missing, blank, unknown, duplicate, or disabled-project mappings fail closed instead of exposing all connections. Generated examples cover Bash, correctly quoted WSL interop, and PowerShell's required `&` call operator. Remote stdout/stderr stream through verbatim and the remote exit code is passed through (124 = timeout, 2 = CLI error); SFTP transfers stream in chunks, credentials stay local, each call is audited, and a hard guard refuses to transfer mini-term's own credential-bearing `config.json`. A machine-wide singleton daemon holds the persistent session pool (auto-spawned on first call, drains after 10 minutes idle, swaps itself out on upgrades); Ctrl+C/client disconnect and request timeout explicitly close the SSH channel while retaining the healthy session. IPC is current-user-only and fails closed if its secure endpoint cannot be created. If the daemon is unreachable the CLI transparently falls back to an in-process connection. During the transition the `mt-ssh-mcp` MCP sidecar still ships.
- **SSH remote projects** — Add a directory on a remote server directly as a mini-term project: the "Add Remote Project" dialog picks a saved SSH connection and takes a remote POSIX path, validating that the directory exists over SSH before saving; the file tree lazy-loads over SFTP (an inline loading spinner on expand, manual refresh, root `.gitignore` filtering); the terminal connects via `ssh -t` and lands straight in the project directory, with a one-click reconnect overlay after a disconnection; the Sessions panel merges the remote machine's Claude / Codex sessions chronologically, with content viewing supported; deleting the referenced connection shows the project in a "broken-link" state rather than failing silently; under the hood it shares the extracted `mt-ssh` crate (persistent russh session pool + SFTP primitives) with the SSH tool sidecars, and remote cache keys mix in the connection id so identical paths on two servers never cross-contaminate.

### WSL Support (Windows)

- **WSL directories as project roots** — Supports adding WSL paths in both `\\wsl$\<distro>\<unix-path>` and `\\wsl.localhost\<distro>\<unix-path>` forms as projects; the displayed path automatically strips the `\\?\UNC\` verbatim prefix, and the file tree expands and previews normally.
- **Automatic wsl.exe launch** — When the cwd is detected as a WSL UNC path, PTY creation ignores the user-configured shell (cmd / pwsh, etc.) and forces `wsl.exe -d <distro> --cd <unix-path>`, so the cwd truly lands inside WSL (`pwd` shows `/home/<user>/proj` rather than `C:\Windows`), consistent with Windows Terminal's `MangleStartingDirectoryForWSL` behavior; the distro name is parsed directly from the path without invoking `wsl -l -v`, and a one-time toast appears when the rewrite triggers.
- **Known limitations** — AI status detection is limited for processes inside the WSL VM, so AI status may stop working. `notify` file watching very likely loses events on the WSL 9P filesystem, so the file tree needs a manual refresh. Verified only on WSL2; WSL1 compatibility is not guaranteed.

### File Search

- **Global search** — Triggered by `Ctrl+Shift+F` (macOS `⌘+Shift+F`) or the file-tree toolbar button, supporting both filename and file-content search modes.
- **Path matching** — In filename mode, a query containing `/` (a Windows backslash works too) is matched against the project-relative path, e.g. `pages/task/my` hits `src/pages/task/my/my.vue`, with the highlight on the path; queries without a separator still match the bare filename only.
- **Regex matching** — Toggle between substring / regex modes, with matched keywords highlighted in the results.
- **Streaming results** — The backend walks the file tree with the `ignore` crate and pushes results in batches every 50 entries or 100ms, cancellable at any time.
- **Content grouping** — Content-search mode groups matched line numbers by file; clicking a result previews and jumps straight to the matched line.

### AI Process Awareness

- **Hook event system** — Integrates the official Claude Code / Codex / Grok Build Hook APIs to receive AI tool events (SessionStart / End, ToolUse, etc.), which is more precise and timely than process polling; the built-in `miniterm-hook` CLI is called by the hook system to POST events to a local server; the settings UI registers / unregisters hooks per CLI via "injection targets" — one checkbox row each for Claude Code / Codex / Grok, with registration and removal acting only on the selected ones (the three config files are unrelated; a user of just one CLI has no reason to get the other two written). Each row shows that CLI's config file path and registration state (not registered / N events registered / outdated N⁄M in yellow, prompting a re-register to pick up newly added events); the default selection is whichever CLIs are already registered (so an old user hitting register is a pure top-up), falling back to all three when none is, preserving the first-run one-click experience. Writes merge rather than overwrite your existing hooks. Codex permission requests stay in `ai-working` through approval and tool execution, avoiding premature completion notifications.
- **Real-time status detection** — Once hooks are reporting they are the status source for that pane; each polling round reads the hook state directly and never consults output activity (a TUI's idle redraws used to read as "working again," firing the completion notification over and over). Panes without hooks fall back to input detection (recognizing typed `claude` / `codex` / `opencode` / `pi` / `grok` commands, with a line-snapshot fallback for ↑ history and Tab completion) plus 500ms output-activity polling, showing idle / working / error states.
- **Grok Build hook integration** — `grok` (xAI's terminal agent) runs on the same hook pipeline as Claude and Codex: status badges, completion announcements, AI launchers, and mobile-initiated sessions all work. Three structural differences are each handled: (1) grok also scans `~/.claude/settings.json` for hooks by default (a Claude compatibility layer), so the same event arrives twice — the sidecar identifies the compatibility-layer copy via `GROK_SESSION_ID` plus "was an argv passed" and drops it, while still letting it through when only Claude hooks are registered (then it's the sole source); the deciding factor is whether the native hook file is present. (2) The command registered into `~/.grok/hooks/` is a **bare filename with no spaces** (the hook binary is copied into that directory at registration), because a command containing spaces is handed to a shell — and on Windows which shell (git-bash / pwsh / powershell / cmd) depends on the environment, with mutually incompatible quoting; the event name travels via grok's injected `GROK_HOOK_EVENT` instead. (3) grok has no `PermissionRequest` event — "waiting for your approval" is a `Notification` of type `permission_prompt`, normalized onto the same amber lamp, while its `task_complete` is an FYI rather than a to-do and lights nothing. One more thing is smoothed over: grok fires an extra `Stop` at session teardown (`reason` of `channel_closed` / `shutdown`), which would otherwise announce a bogus "task complete" every time you quit grok.
- **Grok's session log shape** — Unlike the other two ("one file, one session"), a grok session is an **entire directory**: `{grok_home}/sessions/{URL-encoded cwd}/{session-id}/`, with the transcript in `updates.jsonl` (an ACP session-update stream) and metadata in `summary.json`. Project matching **decodes the directory name** rather than encoding the project path (the latter would mean replicating its encoding crate's escape set byte for byte; for over-long paths that degrade to a `{slug}-{hash}` form, it falls back to the `.cwd` file inside the directory). A single message is streamed to disk as arbitrarily many chunk lines, so chunks must be accumulated until a boundary (tool call, turn completion, the other party speaking) — otherwise one answer shatters into dozens of mirror entries. Usage comes from the `usage` payload on `turn_completed` (broken down per model; ACP's input count folds in cache reads and writes, split into disjoint buckets that reconcile with `totalTokens`). **Tool rankings are empty for grok** — the persisted ACP `tool_call` carries only a human-readable title, never the actual tool name, and substituting the title would pour natural-language labels into the ranking.
- **Agents identified by input detection alone** — `opencode` / `pi` have no hook integration and no parseable local session log: status badges, completion announcements, AI launchers, and mobile-initiated sessions all work, but the conversation mirror, the AI history panel, and usage stats stay empty for them. The mirror's heuristic binding is gated behind a whitelist (`agent_has_session_log` in `mt-relay::mirror`) — anything outside it returns an empty mirror rather than falling back to the newest session file of another agent in the same project and pasting someone else's conversation into that pane. Command matching is an exact basename match, so `pip` / `ping` / `pixi` / `pi.py` are never mistaken for `pi`.
- **Three fallbacks for a stuck badge** — `Stop` simply doesn't fire in several cases: a turn ending on an API error emits `StopFailure` instead (mapped to ai-idle, lighting the amber lamp so you know to come back and resend), and a user interrupt via Esc / Ctrl+C emits nothing at all (settled from input detection, cause=`Interrupt`). Whatever those two miss is caught by a **stall check**: if the hook state sits at ai-working while both the state and the PTY output stay silent for 10 seconds, it converges — to `idle` when an exit was already triggered (Ctrl+D / double Ctrl+C / `/exit`, with no hook event since to prove otherwise), and to `ai-idle` otherwise. All three write their verdict into the hook state **once**, so they converge instead of oscillating, and none of them uses a `Stop` cause, so none is ever announced as a finished task (precisely why the memoryless version of this fallback was removed in v0.9.3). Panes awaiting user approval (Codex's `PermissionRequest`, for one) are exempt from the stall check, which would otherwise wipe out the tray's yellow light along with the badge.
- **Status aggregation** — Aggregated layer by layer from pane → tab → project, with priority `error > ai-working > ai-idle > idle`.
- **Completion notification trio** — Fires the moment an AI task goes working → idle *and* the cause is a `Stop` event (permission requests, notifications, and elicitations also land on `ai-idle` and are no longer misreported as completion; the hookless fallback path still keys off the falling edge alone):
  - A bottom-right toast desktop notification (only for inactive projects, deduplicated per project).
  - A DONE badge in the project list, cleared on click.
  - Taskbar flashing (Windows) / Dock bouncing (macOS), triggered only when the window is unfocused.
  - A notification sound (a built-in synthesized default tone, with support for a custom audio file).
  - All notification toggles are independently configurable, managed together under "Settings → AI → Notifications" (hook registration lives on the sibling "Hook events" page).
- **Awaiting-confirmation alert** — When the AI stops to ask for tool permission, needs an MCP form filled in, or ends a turn on an API error (`PermissionRequest` / `Elicitation` / `StopFailure` — the same rule that lights the project row amber), it fires one more alert through the same channels as above. Independently toggled and on by default (Settings → AI → Notifications → Trigger; it fires far more often than completion, so anyone who only wants completion alerts must be able to turn it off). The trigger is the **rising edge** of the amber light rather than "this cause is an awaiting-type one": the backend deliberately exempts these events from deduplication (a second authorization request in the same turn must not be swallowed), so keying off the cause alone would alert several times for one pending request. While the amber light stays on there are no repeat alerts; typing into that terminal counts as handling it (clearing the light), so only the next request forms a new rising edge. The toast uses the warning color plus an exclamation mark to stay distinct from the green "finished" one, and sets no DONE badge (that marks completion).
- **Tray status light** — A persistent system-tray light for global AI status: yellow = awaiting confirmation, blue = working, green = unread completion, gray = quiet, rotating through coexisting states while the window is unfocused; the right-click tray menu lists **every project with an AI session** and its status (including ⚪ AI-idle ones, not just the busy ones; ordered awaiting > working > done > idle, entry cap configurable, idle entries never light the lamp) and picking a project jumps straight to its most urgent pane, while a left click summons the main window and jumps to the session that needs you next (the same landing logic as the title bar status light; a setting turns the jump off so it only summons the window — Linux offers the right-click menu only). Notification classification only treats permission / confirmation wording as "awaiting" — API errors and retry waits never light yellow. Can be disabled in Settings.
- **Automatic session resume** — After a restart, each split pane automatically writes `claude --resume` / `codex resume` / `grok --resume` to reconnect its previous session: session identity comes from hook reports and persists with the layout across one restart; everything written back is allowlist-checked (alphanumerics plus `-_` only, max length 128), remote panes are excluded, and anything unrecognizable is never written. Can be turned off under Settings → System → General (terminals still come back, they just don't run the resume command).
- **Session enter/exit detection** — Recognizes entering AI via command echo; recognizes exit via a double `Ctrl+C` / `Ctrl+D` or `exit` / `quit` / `:quit` / `/logout`.
- **Session history** — Reads local Claude / Codex / Grok history records, with a right-click to copy the resume command for quick continuation; the first screen renders only 20 entries, with a "Load more" button at the bottom to expand on demand (no longer triggered by scrolling).
- **Session branching** — makes "trying several approaches to one task in parallel" a first-class feature (design: `docs/plans/2026-08-14-session-branch-tree-design.md`). **The fork action**: right-click a pane → "Fork session to new split" — the original session keeps running in place while the new pane split off to the right gets the fork command (Claude `--resume {id} --fork-session`, Codex `codex fork {id}`; command templates live in a capability table with sessionId whitelist validation, so wiring up a new agent means declaring its capability bits; the new pane is a new process, so "allow for this session" permission grants don't carry over). The new PTY starts in the session's recorded cwd when available (`claude --resume` only finds sessions bucketed under their start directory). **The branch tree**: the history panel gains a persisted "flat | tree" toggle where forked sessions hang under their parents with indent lines — lineage comes from pointers the CLIs themselves write to disk (Claude: `forkedFrom.{sessionId,messageUuid}` on copied jsonl lines, message-level; Codex: `session_meta.payload.forked_from_id`, session-level, filtering out subagent threads via `thread_source=="subagent"`), plus **self-bookkeeping** for forks mini-term itself initiated to cover the window before the session file lands on disk (merged with per-child dedup, disk edges winning); dangling parents become roots, cycles are defended against, and the tree builder is pure logic covered by direct unit tests. **Clicking a node**: if a live pane runs that session, switch/activate/focus it; otherwise resume it in a new terminal (WSL/remote-sourced sessions get a notice that they can't be resumed locally). The context menu's "View session branches" entry **expands on hover** into a family-tree panel — the whole family at a glance with a "← current" marker. Vendor icons in the tree and panel follow the session's **latest model** (the backend tail-scans the last 64KB for the newest model name and runs it through the vendor-inference rules, so a claude CLI running GLM/DeepSeek through a proxy lights up the real vendor's icon, falling back to the CLI icon when unrecognized; pane tab CLI icons deliberately stay put). Grok is reserved: no CLI-level fork, so its missing capability bits simply hide the menu entries.
- **Session viewer** — A right-click "View" shows the full conversation, with User as plain text and Assistant rendered as Markdown, supporting `Ctrl+F` search highlighting and quick navigation between User messages.
- **WSL sessions** — Reads Claude / Codex session history inside WSL distros directly from Windows (no `wsl.exe` spawn — via `\\wsl$` UNC plus registry-based distro enumeration): WSL-rooted projects auto-derive the distro and path with zero configuration; Windows-path projects pick a distro via the right-click "WSL Sessions" submenu and are scanned through `/mnt` path mapping, with in-session cwd verification to prevent cross-project mixing; WSL sessions merge chronologically with local ones under a WSL badge, a header spinner shows while loading, and viewing session content is supported too.
- **AI task markers** — Each time the user presses Enter inside an AI session, a marker is dropped in the terminal; the ⚑ button at the tab's top-right drops down the list of past submissions, and clicking one or pressing `Ctrl+Shift+↑/↓` (macOS `⌘+Shift+↑/↓`) jumps between markers, briefly highlighting the target line.

### Usage Statistics

- **Multi-dimensional panel** — The "Stats" button in the top bar opens a panel aggregating Claude Code / Codex / Grok cost, call count, and session count as KPI groups, with daily / hourly trend charts (custom-drawn rendering), model rankings, project rankings, and top sessions; agent / time-range / project filters are one click away, and the custom date range comes with a hand-drawn calendar picker (clamped to the past year).
- **Panel interaction details** — The project filter dropdown pops out anchored to its trigger button and is capped to the viewport height, so it never overflows the screen no matter how many projects there are; anything beyond the cap gets a draggable scrollbar. The refresh button shows its tooltip after a 500ms hover. Scrollbars are reserved for dropdown-style menus like this one (the caller caps their height) — context menus never get one, since a dozen entries cannot scroll anyway and the bar would only add a stray track and gutter.
- **rusqlite local ledger** — Local session JSONL files are parsed into a SQLite ledger; panel queries return in milliseconds while incremental sync catches up in the background, both on open and while the app stays resident (files are re-parsed only when their fingerprint changes). The ledger is positioned as "a cache regenerable from the raw records": corruption triggers an automatic rebuild, and there is no migration burden.
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
- **Hover pane preview** — **Only for projects running an AI session** (the same test as the row's AI brand icons: if the icon is lit, the preview exists; the overlay closes as soon as the AI exits, and hovering a plain shell project just shows the absolute path as a tooltip instead of interrupting you with a card). Hover such a project row for 250ms and a **miniature layout puzzle** of its terminal area pops up: real split proportions reproduced from the SplitNode tree, in a fixed-width overlay that never runs off screen, matching what you'd see after switching; it redraws every 500ms while open, so the preview is live. It's implemented by reading the terminal grid and painting a miniature bitmap — cell contents come through the same rendering pipeline as the main terminal (same-color run extraction, bold standard colors brightened, 256-color / truecolor resolved), drawn onto a cell grid and scaled proportionally, so even a hidden pane's content is available in real time. Each split leaf shows its active tab's picture (bottom-left anchored, preserving the newest output and the TUI input area); hidden tabs are summarized by a "+N" badge carrying the highest-priority status among them (error > ai-working > ai-idle, the same ordering as status aggregation), so AI activity buried in an inactive tab isn't missed; panes without a PTY show a "Not started" placeholder (the project's absolute path stays visible in the card header). **Inactive pane tabs** also pop a single-cell thumbnail overlay after a 250ms hover (same rendering pipeline, redrawn every 500ms while open; the "Not started" placeholder and remote-disconnect veil follow the same conventions), with **no AI gate** — a hidden tab's content is invisible until you switch to it anyway, and the preview answers exactly "what's on that tab right now". The trigger timing matches the project-row preview; it closes on mouse-out / click / context menu / scroll, and the card clamps to the horizontal edges, flipping above the tab when a bottom split leaves no room below.
- **Drag to add projects** — Drag a folder from the file explorer onto the project list to add it quickly, with automatic detection of files / folders / duplicate projects and visual feedback.
- **Nested groups** — Up to 3 levels of project grouping, drag to reorder, collapse / expand, with a group context menu to add either a local project or a remote SSH project directly into that group (a collapsed group expands automatically). "Delete group" asks for confirmation first, explaining that the projects inside move up one level rather than being deleted; "Move to group" expands the group tree level by level as submenus, marking the current group with a ✓ and greying it out, with over-depth groups unselectable.
- **Worktree sub-projects** — A worktree turned into a project is mounted beneath its main project as a sub-project (indented, following the group), and can be dragged out or detached via "Detach from parent" to return to the top level; deleting a parent project promotes its sub-projects in place instead of losing them. The project list shows a ⎇ branch badge for worktree projects, and the repo list and Changes dropdown label worktree entries as well. **Externally removed worktrees are reconciled automatically** — whenever the window regains focus, sub-project directories are probed for existence, so after an AI agent runs `git worktree remove` in a terminal the vanished sub-project is dropped along with its terminal resources and the ⎇ badges are re-probed (cleanup only happens while the parent project directory still exists, so a disconnected drive can't wipe entries; SSH remote and UNC/WSL paths are excluded). "Clean up stale entries" in the worktree modal removes the projects pointing at those worktrees too.
- **File tree** — An integrated directory browser with natural sorting (V1 → V2 → V10 rather than lexicographic), nested `.gitignore` greying (ignore rules and `!pattern` allowlists at every sub-directory level take effect, consistent with git behavior), and live refresh via `notify` file watching.
- **File operations** — Create / rename / delete files and folders and view contents inside the file tree (Markdown rendering, image formats shown directly, and binary / oversized files get a friendly notice).
- **File workbench** — Local and remote files opened from the tree live in main-area tabs alongside the terminal, where you view, edit, and save them: tree-sitter syntax highlighting (30+ languages) matched automatically by file type, basic indentation, find & replace (`Ctrl+F`); `Ctrl+S` saves atomically (temp file + rename, so a bad write can't corrupt the file), and CRLF files round-trip with their original line endings so you never get a whole-file diff; closing with unsaved changes asks first, and external modifications reload silently when clean or show a notice bar when dirty; Markdown preview renders the unsaved draft live; syntax colors follow the theme skin. Remote files are read and written over SFTP: every save is compared against the baseline loaded earlier, and if someone changed the file on the server you can reload or force-overwrite; writes go through temp file + backup + rename with stale backups cleaned up automatically; a failed refresh only shows a banner instead of covering the editor; any remote file can also be downloaded locally.
- **Images in document previews** — Both the Markdown and HTML previews render images: relative paths resolve against the current file's own directory, lines that contain nothing but an image are split out and drawn directly, and the width is the smaller of the image's natural size and the available text width (large images are no longer squashed into a strip by object-fit); SVGs are rasterized at 2x. Remote images (badge rows at the top of a README, linked screenshots) are really fetched through a built-in HTTP client that allows only `file://` and `http(s)://`, with a 10s timeout and a 32MB response cap, kept as a process-wide singleton; if a fetch fails you get a clickable placeholder with the alt text that opens the original in your system browser. Markdown from remote files is untrusted input: it is sanitized on the same GFM AST the renderer uses and re-parsed until a fixed point is reached — raw HTML is shown as source, links are allowed only for http(s) / mailto / tel / anchors, images never load inline (image-only lines fetch on click); remote HTML never enters the preview and is source-view only.
- **HTML preview** — Besides the source editor, `.html` files get a preview mode (simplified rendering, with a "no CSS / no scripts" notice at the top); local targets in `src` / `href` / `poster` are rewritten to `file://` so images and local assets actually show up. The toolbar always carries "Open in browser", which resolves through the **https protocol handler** rather than the `.html` file association (the latter is often set to an editor, so clicking it just opened another editor) — on Windows it reads the UserChoice ProgId for `https`, then its `shell\open\command`, falling back https → http → system-level `HKCR\http`; if no browser is found it reports an error instead of silently falling back to the file association. Paths are escaped for `%`, spaces, `#`, and `?` when converted to URLs.
- **Open in external editor** — A button at the top-right of the file tree opens the current project in your configured editor (VS Code by default), with the path customizable under "Settings → System → Editors"; files can be opened with the system default app.
- **Project-level environment variables** — The project context menu "Environment Variables…" opens a management dialog with a row-level `[enable checkbox][key][value][✕]` layout, injecting per-project variables into the PTY child process when starting that project's terminal; strict POSIX validation (key matches `^[A-Za-z_][A-Za-z0-9_]*$`, no `MINITERM_` prefix, no `WSLENV`, no duplicates within a project, and value forbids `\n/\r/\0`); on top of validation, a defensive `MINITERM_`-prefix + `WSLENV` filter is applied, so even hand-editing `config.json` to bypass the UI validation cannot break the hook protocol or WSLENV concatenation; under WSL projects, variables pass through to Linux bash via the WSLENV mechanism (`/u` is one-way without path translation; an `export` of the same name in `~/.bashrc` will override).

### Git Integration

- **File status** — The file tree shows Git status colors (modified / added / deleted / conflict).
- **Changes / history in one view** — The Git panel stacks two collapsible sections: Changes on top and commit history below, with a draggable divider (clamped 15%–85%) and animated collapse / expand, remembering fold state and ratio for the session; a repo bar at the top of the panel switches repos via a dropdown (worktree entries marked ⎇), clicking the branch badge only switches which branch's history is shown (no checkout, highlighted when viewing a non-HEAD branch), refresh / Pull / Push sit on the same bar, and right-clicking the repo name opens it in a terminal or enters worktree management.
- **Change diff** — A detailed diff of working-tree file changes, parsed at the hunk/line level, with side-by-side / inline dual views; side-by-side mode supports dragging to adjust the split ratio, and the font size follows the terminal font setting. Both diff dialogs (single file and commit) are 80vw × 85vh, exactly matching the usage-stats panel frame; long lines scroll horizontally, the two side-by-side columns scroll in sync, `@@` hunk headers act as separators, and prev / next-change jumps are available. The LCS backtrack tie-break is fixed (delete / add lines previously never paired up and drifted a row apart), and paired lines get word-level highlighting that paints only what really changed; the diff-size heuristic now measures the middle section after stripping the common prefix and suffix, so a one-line change in a several-thousand-line file no longer degrades into "whole block replaced".
- **Commit history** — A flat list of the commit log for the repo selected in the top repo bar, with cursor-based pagination (30 entries by default).
- **Branch topology graph** — Each history row draws an SVG topology graph on the left, laying out branch, merge, and pass-through lines by lane, coloring nodes per lane and marking merge commits with a filled dot inside an outer ring; merge-in lines use the branch's own color as a Bézier curve that gradient-blends into the mainline at its root. The backend revwalk appends TOPOLOGICAL sorting so clock skew or a rebase can't place a parent after its child and break the lines, and a commit row is only labeled with the branches this repo itself has checked out, rather than hanging every other worktree / remote branch on it.
- **Commit diff** — View the file changes of any commit, switching file by file.
- **Branch info** — Local / remote branch lists.
- **Source control panel** — A VS Code-style Changes panel grouping Staged / Changes / Untracked, supporting per-file and bulk stage / unstage / discard, `Ctrl+Enter` to commit quickly, and toggling between list and tree views.
- **Pull / Push** — Buttons on the top repo bar sync with the remote in one click, with a refresh button to reload the commit log and branch info.
- **Multi-repo discovery** — Automatically scans all Git repos under the project directory (recursing 5 levels, skipping `node_modules` etc.).
- **Worktree management** — Right-click a project or the repo bar at the top of the Git panel to open the "Worktree management" dialog: list every worktree, create one from an existing branch or a new branch, remove it (force optional), and prune stale entries, with the repo list refreshing immediately after any change; a worktree can be turned into a project in one click or opened directly in a terminal, and panes support a working-directory override that persists with the layout and is inherited by splits. When the project root itself isn't a repo, it scans downward for sub-repos and groups them by main worktree into a list whose group headers are checkable (multi-select / select-all), creating one worktree per checked repo in a single action — the branch dropdown then offers the intersection of all repos' branches, the path field becomes a parent directory previewing the `<repo>-<branch>` landing spot, and failures are listed per repo.

### Appearance & Configuration

- **Icon sidebar + three-column layout** — A persistent icon bar on the far left (collapse middle column / Sessions / Git / Settings / SSH); the middle column stacks Projects over Files and collapses as a whole; the terminal sits on the right. Sessions / Git are floating drawers that slide out from the right edge over the terminal (mutually exclusive single panel, left-edge drag to resize with persisted width, ✕ to close), with a blue vertical bar indicating the active state.
- **Three theme modes** — Auto (follows the system) / Light / Dark, with Dark based on a Warm Carbon palette; the title bar is drawn by the app and colored from the theme, with no first-frame light flash for dark-mode users on startup.
- **Custom title bar** — The frameless window gets a self-drawn top bar: app name, version, project switcher and global status light on the left, window controls on the right, colored from the theme instead of the system's grey strip. Adapted to each platform's conventions:
  - **Windows / Linux** — Minimize / maximize / close on the right, with the close button turning red on hover. Win11 **Snap Layouts** still work: hovering the maximize button pops the snap menu.
  - **macOS** — The native traffic lights are kept, with space reserved in the top-left corner; no hand-drawn dots, so full-screen, gestures, and system integration all survive. Closing the last window quits the process instead of leaving an unresponsive zombie icon in the Dock.
  - **Project switcher** — A pill button next to the version number, set off by a divider, always showing the current project's name with its own AI status dot (dimmed when it has no AI session). Its dropdown lists every project with an AI session and its status (the same aggregation the tray menu uses, ordered awaiting > working > done > idle); clicking a project switches to it and lands on its most urgent pane, or just switches when everything is quiet.
  - **Global status light** — Sits right beside the project switcher and aggregates the most urgent state across every pane of every project (error > awaiting confirmation > working > done). Clicking jumps to the session that needs you next: awaiting-confirmation / errored first, then the **earliest finished** one, and only then anything still running. This deliberately differs from the tray context menu's ordering — the tray answers "which projects are still alive", the status light answers "what should I do next".
  - Dragging and window controls go through GPUI's native WindowControlArea; double-clicking the bar toggles maximize / restore.
- **External theme packs (Dream Skin-compatible)** — Settings → Appearance → Theme & language can import third-party skins from a folder or a zip into `{app_data_dir}/themes/<themeId>/` (`theme.json` required; `theme.css` / background image optional). "Create example" in the same section writes a ready-to-edit sample skin into `themes/example/` (`theme.json` + `theme.css` + a `README.md` documenting every field; save an edit and it hot-reloads). The sample is literally the **same file** as [`docs/theme-pack-example/`](theme-pack-example/) in the repo (embedded at compile time via `include_str!`, so docs and product can't drift apart), and it errors instead of overwriting when the folder already exists — a copy you've edited is never silently wiped. When the pack ships a `manifest.json`, every file is checked against its bytes + sha256 to catch corruption; imports land in a staging directory first and are swapped in atomically only after validation, so a bad pack can't take out an existing skin of the same name. A pack's light/dark nature is fixed by its author via `appearance` in `theme.json`, and the built-in theme buttons show as unselected while it's active. Editing a file in the pack hot-reloads it. A pack may declare a background image, in which case terminal surfaces turn translucent over that ambient layer, and the settings-page card shows a live thumbnail. An imported `theme.css` and the `tokens` overrides in `theme.json` pass through the same external-reference gate: no `@import`, and any reference pointing outside the pack rejected — the check runs on a sample with comments stripped and CSS escapes resolved, inspecting both `url()` and bare string literals like `image-set("…")`, so escaped forms such as `url(\68 ttps://…)` are caught too.
- **Independent font tuning** — The UI and terminal font sizes (10-20px) / families are adjustable separately, and the terminal can optionally follow the UI theme. The default family is chosen per platform — Cascadia Mono on Windows, Menlo on macOS, DejaVu Sans Mono on Linux — each with CJK and emoji fallbacks, so a missing primary font never degrades into a proportional face.
- **Terminal ligatures** — The "Enable terminal ligatures" toggle under Settings → Appearance → Font (off by default) makes `=>`, `!=`, `->` and friends merge per the font's own ligature rules. Merged runs are shaped in one pass with the run origin pinned to `cell_width × start column`, so as long as the ligature preserves total width the characters still land on the column grid; if the shaped width doesn't equal "columns × column width", the run is reshaped once with ligatures disabled, guarding against fonts whose ligatures don't preserve width. Note the default family, Cascadia **Mono**, is the de-ligatured cut — switch to Cascadia Code, Fira Code, or similar to see any effect.
- **Layout persistence** — Split ratios, tabs, and window size / position are saved automatically and restored on restart.
- **Close confirmation** — Closing the window takes stock of AI sessions only (panes in ai-working / ai-idle); plain shell terminals no longer count, and the confirmation appears only when AI sessions exist, listing their names. All project layouts are flushed either way.
- **Update check** — Fetches the GitHub Release on startup; when a new version is available a highlighted hint appears on the icon sidebar (click to download), and the version number is written into the native window title.
- **Bilingual UI (English / 中文)** — A one-click language toggle under "Settings → Appearance → Theme & language" instantly re-renders the entire interface; the language is auto-detected from the system on first launch and remembered across restarts. Every page and feature is fully translated, with a lightweight built-in i18n layer (no extra runtime dependency).
- **Settings center** — A unified settings panel whose sidebar is a two-level "group + page" menu: Terminal (Shell / Copy & paste), Appearance (Theme & language / Font), AI (Notifications / Hook events), System (General / Editors), with Shortcuts and About kept at the top level. Grouping by topic keeps every page to roughly one screen, ending the old "nine control groups on one page, scroll half a page to find a toggle" problem.
- **Icons everywhere** — File-type / folder icons in the file tree (including open-folder states), AI brand icons and tech-stack icons on project rows — official brand SVG shapes, natively drawn.
- **Startup performance** — Native rendering with no web assets: the startup path makes zero network requests, so the offline first frame is unaffected (the price table refreshes daily and falls back to its cache when unreachable); a unified startup-timeline trace is written to stderr for regression hunting.
- **Interface motion** — Dialogs, context menus, and the side drawer share one enter/exit animation: the backdrop fades in while the panel drops and scales into place; on close it plays the reverse before unmounting (content is frozen meanwhile, so it never goes blank mid-fade or keeps swallowing Esc). Context menus expand from the cursor, and switching terminals or creating a split each get their own transition. When the system turns window animations off these transitions still play — the usage panel's number tweens and chart animations are exempted likewise — only looping animations such as the blinking status dot are stopped.

## Tech Stack

The whole application is **native Rust** (the earlier Tauri + React build was removed; its source lives in git history):

| Layer | Implementation |
|---|---|
| Shell / rendering | GPUI 0.2 (the framework behind Zed — GPU-native rendering, single process, no WebView) |
| UI | Pure Rust: gpui-component + hand-drawn widgets |
| Terminal | alacritty_terminal (in-process VT parsing — zero IPC, zero serialization) · portable-pty |
| State / layout | Single store · recursive SplitNode tree |
| Git / files | git2 (libgit2) · notify + ignore |
| Usage stats | rusqlite local ledger · hand-drawn trend charts |
| Mobile relay | axum + tokio WebSocket (`relay-server/`) · React + Vite PWA (`mobile/`) |
| Tests | **1,672 Rust tests** (28 test targets) + relay-server protocol boundary tests |

## Getting Started

### Direct Download

Head to [Releases](https://github.com/dreamlonglll/mini-term/releases) to download — three platforms:

- **Windows x64 (primary supported platform)** — `Mini-Term_*_x64-setup.exe` installer (NSIS, per-user install without admin rights; upgrades in place if an older version is installed)
- **macOS arm64** — `Mini-Term_*_aarch64.dmg`
- **Linux x64** — `Mini-Term_*_amd64.deb` or `Mini-Term_*_amd64.tar.gz`

> **Platform support note**
> - **Windows** — The primary supported platform with guaranteed usability; daily development and testing all happen on Windows.
> - **macOS / Linux** — Supported at the code level, but **not well polished**, lacking thorough refinement; Issue reports are welcome.

#### macOS Installation Note

After downloading the `.dmg` and double-clicking to open it, if the system shows **"Mini-Term" is damaged and can't be opened. You should move it to the Bin**, the file is not actually corrupted — the Release artifact simply isn't signed with an Apple Developer ID and is rejected by Gatekeeper due to the quarantine flag.

Drag the `.app` into `/Applications`, then run this once in a terminal to lift the restriction:

```bash
xattr -cr /Applications/Mini-Term.app
```

After that it launches normally on double-click. You'll need to run it again after each version upgrade.

### Build from Source

#### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) >= 1.95
- [Node.js](https://nodejs.org/) >= 20 — used only by the sidecar staging script (standard library only, no npm dependencies)

#### Build & Run

```bash
# Clone the repo
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term

# Build the three sidecars and stage them (plus the portable ConPTY) into target/debug/
node scripts/stage-sidecars.mjs

# Run in development
cargo run -p mt-app

# Release build (output: target/release/mini-term(.exe))
cargo build --release -p mt-app
```

> Hook reporting and the portable ConPTY locate sidecars and resources **next to the exe** — the release bundles ship them all. For the full experience when running from source, run `stage-sidecars.mjs` once first (use `--release` for release builds, which stages into `target/release/`).

## Project Structure

```
mini-term/
├── crates/                       # Main workspace (12 crates)
│   ├── mt-app/                   # GPUI app shell: Workspace component tree, AppStore global state, SplitNode layout tree, panels / dialogs / tray / title bar
│   ├── mt-ui/                    # GPUI rendering layer: terminal view / element, theme bridge (no business logic)
│   ├── mt-terminal/              # VT state machine + grid model (alacritty_terminal wrapper, gpui-free)
│   ├── mt-pty/                   # PTY lifecycle (spawn / read / write / resize / kill) + portable ConPTY preload
│   ├── mt-ai/                    # AI awareness: hook server (authoritative), hook registration, input-detection fallback, status verdicts, session record reading
│   ├── mt-project/               # File tree, directory watching, search, Git (git2), external editors, WSL distro enumeration
│   ├── mt-config/                # Config persistence & theme packs (gpui-free)
│   ├── mt-i18n/                  # Bilingual copy layer (dictionary source in locales/*.ts; dict.rs is generated)
│   ├── mt-relay/                 # Mobile relay, desktop side: outbound WSS link, pairing, project snapshots / deltas, conversation mirror, command write-through
│   ├── mt-ssh/                   # Shared SSH layer (persistent russh session pool + SFTP primitives, used by both the app and sidecars)
│   ├── mt-usage/                 # Usage stats: session turn parsing / SQLite ledger / aggregation / pricing
│   └── mt-core/                  # Leaf shared library (WSL UNC parsing / SSH prompt scanning / atomic writes, etc.)
├── sidecars/                     # Standalone workspace for the sidecar binaries (independently versioned, not released with the app)
│   ├── miniterm-hook             # Hook CLI tool (called by AI tool hooks)
│   ├── mt-ssh-cli                # SSH CLI (called by terminal AI agents via Bash; daemon-backed persistent pool)
│   └── mt-ssh-mcp                # SSH MCP server (rmcp stdio; transition-period legacy channel)
├── relay-server/                 # Self-hosted relay service (standalone Rust workspace)
│   ├── protocol/                 # Protocol message crate shared by desktop and relay (JSON over WebSocket)
│   ├── server/                   # axum relay service (forward-only, no persistence + PWA static hosting)
│   └── docker-compose.yml        # Build and run from source in one command
├── mobile/                       # Mobile PWA (React + TS + Vite — pairing / list / mirror / commands / start / rename)
├── scripts/
│   ├── stage-sidecars.mjs        # Builds the sidecars and stages them (plus the portable ConPTY) next to the app exe
│   └── stage-conpty.mjs          # Downloads, verifies and stages the pinned ConPTY runtime (Windows)
├── tests/                        # Node-side tests (2 files: ConPTY bundling / vendored-openssl guard)
└── docs/                         # Documentation (feature list / relay deployment / theme pack example, etc.)
```

## Architecture Overview

### Data Flow (single process, no IPC)

```
User keystroke → terminal pane write → AI input awareness → PTY write
PTY reader thread → feeds the VT state machine (alacritty_terminal) directly → wakes a redraw → UI samples the grid per frame
Hook reports / 500ms polling → status verdict → AppStore → status dots / tray / project list
File change notify → file tree refresh
Layout / config change → debounce → config flushed to disk
ai-working → ai-idle(Stop)          → Toast + DONE badge + taskbar attention
attention rising edge(PermissionRequest…) → Toast(warning) + sound + taskbar attention
```

A single-process architecture with no IPC interface layer — the original Tauri build's 16ms batch buffer, bounded channels, dual-watermark backpressure, and orphan-PTY reclamation were all built for the WebView IPC boundary and died with the architecture.

### Status Priority

Terminal pane status is aggregated from leaf nodes up to the tab and project levels:

```
error > ai-working > ai-idle > idle
```

### Component Tree

```
Root (gpui-component root, hosts the Dialog / notification layers)
 └─ Workspace (holds the AppStore and the column views)
     ├─ background_art (theme-pack background image, window level)
     ├─ ActivityBar (44px narrow icon rail)
     ├─ h_resizable three columns (draggable, ratios persisted)
     │   ├─ Middle column (collapsible as a whole · v_resizable into two stacked blocks)
     │   │   ├─ Top:    ProjectList (projects + nested groups + DONE badge)
     │   │   └─ Bottom: FileTree (directory browsing + Git status + file operations)
     │   ├─ TerminalArea (SplitNode tree → nested resizables; leaf = tab bar + terminal pane entities)
     │   └─ SessionPanel (AI history, right drawer, collapsible)
     ├─ UsagePanel (usage statistics overlay)
     ├─ Dialog layer (Settings / SSH / Mobile and other modals)
     └─ Notification layer (completion / awaiting-confirmation toasts)
```

## Recommended Dev Environment

- [VS Code](https://code.visualstudio.com/) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Contributing

Issues and PRs are welcome. External contributions are merged after functional verification and a security review.

Before submitting, please run:

```bash
# Workspace-wide Rust tests (28 test targets, 1,672 cases)
cargo test --workspace

# Node-side tests (just 2 files: ConPTY bundling / vendored-openssl guard)
node --test "tests/*.test.cjs"

# Relay server tests (standalone workspace)
cd relay-server && cargo test
```

## Community

Learn AI, join the L site — [LinuxDO](https://linux.do/)

<p align="center">
  <img src="docs/icon.png" width="128" height="128" alt="Mini-Term Logo">
</p>

<h1 align="center">Mini-Term</h1>

<p align="center">
  <strong>A desktop terminal manager built for the AI era</strong><br>
  Multi-project · Tabs · Recursive splits · AI status awareness · SSH remote · Git worktrees · Watch your AI from your phone
</p>

<p align="center">
  <a href="README.md">简体中文</a> · <strong>English</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-1.0.5--beta-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/GPUI-native-8A2BE2" alt="gpui">
  <img src="https://img.shields.io/badge/Rust-1.95%2B-dea584" alt="rust">
</p>

<p align="center">
  <a href="https://github.com/dreamlonglll/mini-term/releases">Download</a> ·
  <a href="docs/features.md">Full feature list</a> ·
  <a href="docs/deploy-relay.md">Relay deployment</a>
</p>

> **The GPUI-native build is now the only form**: Rust-native rendering, single process, no WebView2 dependency. The earlier Tauri + React implementation was removed from the repository and discontinued after v1.0.0-beta (old installers remain downloadable on past Releases; the source lives in git history).

---

## A familiar situation

You have four Claude Code sessions running, spread across three projects. **Which one finished? Which one is waiting on your approval?** Your system terminal won't tell you — you have to click through them one by one. And firing up VS Code or IDEA just for this trades a few hundred megabytes of RAM for a terminal window.

That is what Mini-Term is for. Status lights in the project list update live; the instant an AI task finishes you get a toast, a taskbar flash, and a sound. And when you're out of the house, your phone shows you the same live view — and lets you send the next instruction straight to it.

![Main UI](docs/screenshots/main.png)

---

## Eight things worth trying

### 🔔 Know the moment your AI is done

Not by guessing at process names — Mini-Term plugs directly into the **official Claude Code / Codex / Grok Build Hook APIs**. Events are reported in real time, which is both more accurate and faster than polling (process polling is kept as a fallback). Hooks are registered / unregistered **per CLI** in Settings, so using only one of them never writes config into the other two, and whatever is written merges with rather than overwrites your existing hook config.

Status aggregates layer by layer from pane → tab → project. The moment a task flips to finished, four things fire, each independently toggleable:

- A bottom-right toast (only for inactive projects, deduplicated per project)
- A **DONE** badge in the project list
- Taskbar flashing (Windows) / Dock bouncing (macOS), only when the window is unfocused
- A notification sound (a built-in synthesized tone, or your own audio file)

When the AI stops to **ask for tool permission**, needs an MCP form filled in, or ends a turn on an API error, the same alerts fire (the toast turns amber, no DONE badge) — that class fires far more often than "finished" and can be turned off on its own. Once the window is out of sight, the **status bar icon** takes over (yellow = awaiting confirmation, blue = working, green = unread completion, gray = quiet): left-click to land on the session that needs you most, right-click for every project with an AI session and its status.

Badges don't get stuck: the cases where `Stop` simply doesn't fire (a turn ending on an API error, you hitting Esc to interrupt) are each covered by their own official event, and on top of that sits a stall check — when both the status and the terminal output have been silent for 10 seconds, the badge comes down. The fallback's verdict is written once and never oscillates, so there is no repeat of the early-version behavior where one task announced itself complete every twenty-odd seconds.

**Grok Build** is a first-class citizen alongside Claude and Codex — status, mirror, history, and usage stats all work. **opencode and pi** are recognized by detecting the command you type, so status lights, completion announcements, and phone-side commands work the same; what they don't have is a parseable session log, so the conversation mirror, AI history panel, and usage stats stay empty for them.

### 📱 Watch your desktop AI from your phone, anywhere

This is probably the most distinctive thing Mini-Term does.

Fill in your relay address in the top-bar "Mobile" panel → save & connect → generate a pairing QR code. **Point your phone camera at it and the PWA opens and pairs itself.** From then on, while you're away you can:

- See **active AI sessions grouped by project**, with status lights synced live with the desktop
- Tap into any session for a **live conversation mirror** — Markdown-rendered replies, scroll up to page in older messages
- **Send commands** from the input box at the bottom — equivalent to typing it on the desktop keyboard and pressing Enter, with an immediate receipt
- **Start a brand-new session from your phone**: pick a project → pick an AI launcher, and the desktop brings the agent up in a background tab
- **Rename a session** to something you'll recognize — the name shows up on the desktop tab too

The security boundaries were designed on purpose: pairing codes are single-use and valid for 10 minutes, pairing a new device replaces the old one, and "Reset pairing" revokes every credential instantly. **The relay forwards and never persists**, with metadata-only logs. And an AI launcher's **command text never passes through the phone or the relay** — the phone references launchers by id and only ever sees the name.

> **Prerequisite**: the relay runs on **your own** server (1 vCPU / 1 GB is plenty, one Docker command to start, plus a domain pointed at it for TLS). That's deliberate — there is no third-party service in the middle. See the [deployment guide](docs/deploy-relay.md).

### 📊 See what your AI spent this month, at a glance

The "Stats" panel in the top bar aggregates Claude Code / Codex / Grok **cost, calls, and sessions** across every dimension: daily / hourly trend charts, model and project rankings, top sessions, with ranges and scopes one click away.

Data is parsed from your local session records into a **rusqlite ledger** — the panel answers in milliseconds while incremental sync catches up in the background. Forked-session history is **never double-billed**, and cache reads / writes are priced precisely at the official rate differentials. The price table refreshes daily from models.dev (a read-only public price list — **no usage data is ever uploaded**); if it can't be fetched, the cache is used — you're never shown made-up numbers.

### 🔁 Restart without losing your AI sessions

Close Mini-Term and open it again: the Claude / Codex / Grok session that was running in each split pane **resumes automatically via `--resume`** — session identity comes from hook reports and persists with the layout. An allowlist guards everything written back into the terminal: unrecognizable ids are never written, remote panes are excluded — better to not resume than to type the wrong command. Don't want it typing commands for you? One switch in Settings turns it off — terminals still come back, they just don't run the resume.

### 🧰 Turn your SSH connections into tools your AI can call

Right-click a project → "Link SSH", tick the connections, and it's enabled for that project — with **visibility scoped to exactly the ones you ticked**. Enabling generates a `SKILL.md` for Claude and one for Codex (each embedding the CLI's absolute path and a random per-project capability token), so the agent loads the skill only when it needs it — no tool schema sits in the context window permanently, and since it's a plain command line, it composes with `grep`, pipes, and redirection.

The built-in `mt-ssh-cli` sidecar provides four subcommands — `list`, `exec`, `upload`, `download`. Remote stdout / stderr and exit codes are **streamed through verbatim**, transfers go over **SFTP in streamed chunks** (constant memory, large files work), credentials never leave your machine, and every call is written to an audit log. **Every command must carry the project token** — missing, unknown, or belonging to a disabled project all fail closed, never falling back to "sees every connection". Behind the CLI is a **machine-wide singleton daemon** holding the persistent connection pool: the first call spawns it and does one handshake + auth, every command after that costs just one RTT, and it drains and exits after 10 idle minutes; if the daemon is unavailable the CLI falls back to an in-process direct connection with an identical contract. There's also a hard guard that refuses to ever transfer mini-term's own `config.json`.

> The `mt-ssh-mcp` MCP sidecar still ships during the transition and is scheduled for removal next cycle.

### 🌐 Remote directories as local projects — and WSL too

- **SSH remote projects** — add a directory on a server as a project directly: the file tree lazy-loads over SFTP, the terminal connects via `ssh -t` and lands straight in the project directory, a one-click overlay reconnects after a drop, and the remote machine's Claude / Codex history is readable with full content. Remote cache keys mix in the connection id, so identical paths on two servers never cross-contaminate
- **WSL support** — `\\wsl$\<distro>\<path>` works as a project root, launching switches to `wsl.exe --cd` automatically so `pwd` really lands inside WSL instead of `C:\Windows`; Windows can also read Claude / Codex session history from inside WSL distros directly

### 🪟 Multi-project · recursive splits · session history

- A **project sidebar** for multiple workspaces, with up to 3 levels of nested groups, drag-to-reorder, and drag-a-folder-from-Explorer to add
- **Arbitrarily nested horizontal / vertical splits**, drag to adjust ratios; tabs, splits, and window geometry all persist and restore on restart
- **Drag panes to rearrange & maximize** — drag a tab into another group to merge, or onto a terminal-area edge to split off a new pane, with a live drop preview; double-click the tab bar's empty area to temporarily fill the terminal area, and content survives throughout
- **Terminal caching** — switching projects, tabs, or panes never rebuilds the terminal instance; lazy startup creates a PTY only for the visible pane, so more history projects never means a slower launch
- **Configurable scrollback** (10,000 lines by default, lowering it takes effect immediately and frees the memory) with correct CSI 3J handling; the Windows build bundles a pinned official ConPTY runtime
- **AI session history** — read local Claude / Codex / Grok records, right-click to copy the resume command, or read the full conversation right there (Markdown rendering + `Ctrl+F` search)
- **AI session branch tree** — right-click a pane and "Fork session to new split": the original keeps running in place, while the new pane holds a copy of the conversation. The history panel gains a "branch tree view" where forked sessions hang under their parents with indent lines, running nodes carry a status dot, and clicking any node either jumps to its live pane or resumes it in a new terminal
- **AI task markers** — every Enter inside a session drops a marker; `Ctrl+Shift+↑/↓` jumps between past submissions

### 🌿 Git integration + batch worktree management

A VS Code-style **Changes panel** (Staged / Changes / Untracked groups, per-file or bulk stage / discard, `Ctrl+Enter` to commit), side-by-side and inline diff views (horizontal scrolling for long lines, vertically synced columns, `@@` hunk separators and prev / next-change jumps, plus word-level highlighting on paired delete / add lines), cursor-paginated commit history, and a **hand-drawn SVG branch topology graph**. The Git panel stacks two collapsible sections — Changes on top, commit history below — visible at the same time with a draggable divider; a repo bar at the top switches repos, the branch badge switches which branch's history is shown (no checkout), and refresh / Pull / Push live on the same bar.

**Worktree management** is especially handy for running several agents in parallel: when the project root isn't a repo itself, it **scans downward for sub-repos** and groups them by main worktree, with checkable group headers so you can **create one worktree per checked repo in a single action**. Any worktree can be turned into a project in one click — mounted under its parent — or just opened in a terminal. **When an AI agent deletes a worktree from the terminal**, the list reconciles itself the moment the window regains focus: sub-projects whose directory is gone are removed along with their terminal resources, leaving no stale entries.

---

## And a pile of details tuned for working alongside AI

| | |
|---|---|
| **Long-text paste** | Clipboard text ≥10 lines or ≥2000 chars is spilled to a temp `.txt` and pasted as a quoted path — your AI tool never has to swallow a wall of text |
| **Image paste** | Screenshots in the clipboard are detected, saved as a temp PNG, and pasted as a path; handles non-standard formats like PinPix |
| **Remote-aware landing** | Both of the above remap in remote terminals: SSH projects upload over SFTP and paste the **remote** path; WSL projects rewrite `C:\...` into `/mnt/c/...` |
| **File drag & drop** | Drag from the file tree or Explorer onto the terminal to insert a quoted absolute path, landing in the exact split pane |
| **Built-in file editor** | Click any file in the tree to edit in place: tree-sitter syntax highlighting (30+ languages), find & replace, atomic `Ctrl+S` saves, external-change detection |
| **Documents preview with images** | Images actually render in the Markdown / HTML preview: relative paths resolve against the file's own directory, and remote images are fetched for real (10s timeout, 32MB cap, every other scheme refused). HTML previews also get an "Open in browser" button that resolves through the https protocol handler rather than the `.html` file association |
| **Global search** | `Ctrl+Shift+F` for filename or content search, substring or regex, streamed from the backend and cancellable anytime |
| **Per-project env vars** | Injected into the PTY child process per project, with strict POSIX validation and a second defensive filter on the Rust side; passes through to WSL via WSLENV |
| **Smart Ctrl+C/V** | Optional: copy when there's a selection, interrupt the program when there isn't; large Windows pastes are chunked so ConPTY doesn't drop lines |
| **Icons everywhere** | File-type icons in the tree, AI brand icons and tech-stack icons on project rows (official brand SVG shapes, natively drawn) |
| **Dwell-to-copy selection** | Hold the mouse still after drag-selecting and the selection is copied with a "Copied" tip; dwell time configurable (0 = off) |
| **Project descriptions** | Right-click to add a gray one-liner next to the project name — tell a row of worktree sub-projects apart at a glance |
| **Zero network requests at startup** | Native rendering, no web assets — startup makes no network request at all (the price table refreshes daily and falls back to its cache) |
| **Flood-proof UI** | PTY bytes feed the VT state machine on a background thread while the UI samples the grid per frame — single process, zero IPC, no intermediate buffer to pile up, so `cat`-ing a huge file can't drag the interface down |
| **Terminal ligatures** | One toggle: `=>` `!=` `->` merge per the font's ligature rules while column alignment stays intact. Note the default Cascadia **Mono** is the de-ligatured cut — switch to Cascadia Code or Fira Code to see any effect |
| **Three themes** | Auto / Light / Dark (Warm Carbon); the title bar matches the theme, with no light flash on startup |
| **External theme packs** | Dream Skin-compatible skins: import from a folder or a zip, sha256-verified against the manifest, hot-reloaded when you edit a file. A pack can ship its own background image, in which case the terminal goes translucent over that ambient layer. External references all pass the same gate (no `@import`; anything pointing outside the pack is rejected). Hit "Example" to drop a ready-to-edit sample skin into the skins folder |
| **Custom title bar** | Frameless window with a self-drawn title bar that follows your theme; window controls on the right for Windows / Linux (Win11 Snap Layouts still work), native traffic lights kept on macOS. Next to the version number sits a **project switcher**, with the global status light beside it — click to jump to the next session needing you |
| **Hover preview for project rows** | **Only pops up for projects running an AI session**: hover for 250ms and a **miniature layout puzzle** of its terminal area appears, split panes reproduced at their real proportions and redrawn every 500ms while open so it stays live; hidden tabs are summarized by a "+N" badge carrying the highest-priority status among them. Inactive pane tabs also pop a single-cell thumbnail on hover |
| **Bilingual UI** | One click re-renders the whole interface in English / 中文, auto-detected from the system on first launch; in-house lightweight i18n, no extra runtime dependency |
| **Grouped settings panel** | A two-level sidebar: Terminal, Appearance, AI, System — every page fits on one screen instead of scrolling half a page to find a toggle |

---

## Tech stack

The whole application is **native Rust** (the earlier Tauri + React build was removed; its source lives in git history):

| Layer | Implementation |
|---|---|
| Shell / rendering | GPUI 0.2 (the framework behind Zed — GPU-native rendering, single process, no WebView) |
| UI | Pure Rust: gpui-component + hand-drawn widgets |
| Terminal | alacritty_terminal (in-process VT parsing — zero IPC, zero serialization) · portable-pty |
| State / layout | Single store · recursive SplitNode tree |
| Config / layout persistence | rusqlite (`config.db` for settings · `layout.db` for the UI layout) |
| Git / files | git2 (libgit2) · notify + ignore |
| Usage stats | rusqlite local ledger · hand-drawn trend charts |
| Mobile relay | axum + tokio WebSocket (`relay-server/`) · React + Vite PWA (`mobile/`) |
| Tests | **1,514 Rust tests** (28 test targets) |

---

## Getting started

### Download

Grab the latest build from [Releases](https://github.com/dreamlonglll/mini-term/releases) — three platforms:

- **Windows x64 (primary platform)** — `mini-term-gpui-*-windows-x64-setup.exe` installer (NSIS, per-user install without admin rights; upgrades in the same directory, and **uninstalls the old build first** instead of overwriting files)
- **macOS arm64** — `mini-term-gpui-*-macos-arm64.dmg`
- **Linux x64** — `mini-term-gpui-*-linux-amd64.deb` or `mini-term-gpui-*-linux-x64.tar.gz`

> **Platform support**
> - **Windows** — the primary platform with guaranteed usability; all daily development and testing happens here
> - **macOS / Linux** — supported at the code level but **not well polished**; Issue reports are welcome

If macOS says "is damaged and can't be opened" on first launch, the file isn't actually corrupt — the Release artifact just isn't signed with an Apple Developer ID, so Gatekeeper rejects it. Drag the `.app` into `/Applications` and run this once:

```bash
xattr -cr /Applications/Mini-Term.app
```

### Build from source

Requires Rust >= 1.95; the sidecar staging script needs Node.js >= 20 (standard library only, no npm dependencies).

```bash
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term

node scripts/stage-sidecars.mjs      # build the three sidecars and stage them (plus portable ConPTY) into target/debug/
cargo run -p mt-app                  # dev
cargo build --release -p mt-app      # output: target/release/mini-term(.exe)
```

> The app locates its sidecars and the portable ConPTY runtime **next to the exe**. The release bundles ship them all; when running from source, run `stage-sidecars.mjs` once first (use `--release` for release builds, which stages into `target/release/`).

---

## More

- 📖 **[Full feature list](docs/features.md)** — every feature in detail, plus architecture overview and known limitations
- 📱 **[Relay deployment guide](docs/deploy-relay.md)** — the self-hosted relay behind the mobile features
- 🐛 **[Issues / PRs](https://github.com/dreamlonglll/mini-term/issues)** — external contributions are merged after functional verification and a security review

Learn AI, join the L site — [LinuxDO](https://linux.do/)

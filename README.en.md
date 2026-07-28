<p align="center">
  <img src="src-tauri/icons/icon.png" width="128" height="128" alt="Mini-Term Logo">
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
  <img src="https://img.shields.io/badge/version-0.8.3-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/Tauri-v2-orange" alt="tauri">
  <img src="https://img.shields.io/badge/React-19-61dafb" alt="react">
  <img src="https://img.shields.io/badge/Rust-2021-dea584" alt="rust">
</p>

<p align="center">
  <a href="https://github.com/dreamlonglll/mini-term/releases">Download</a> ·
  <a href="docs/features.md">Full feature list</a> ·
  <a href="docs/deploy-relay.md">Relay deployment</a>
</p>

---

## A familiar situation

You have four Claude Code sessions running, spread across three projects.

**Which one finished? Which one is waiting on your approval?** Your system terminal won't tell you — you have to click through them one by one. And firing up VS Code or IDEA just for this trades a few hundred megabytes of RAM for a terminal window.

That is what Mini-Term is for. Status lights in the project list update live; the instant an AI task finishes you get a toast, a taskbar flash, and a sound. And when you're out of the house, your phone shows you the same live view — and lets you send the next instruction straight to it.

![Main UI](docs/screenshots/main.png)

---

## Six things worth trying

### 🔔 Know the moment your AI is done

Not by guessing at process names — Mini-Term plugs directly into the **official Claude Code / Codex Hook APIs**. SessionStart / ToolUse and friends are reported in real time, which is both more accurate and faster than polling (process polling is kept as a fallback). One click in Settings registers or unregisters the hooks, **merging with rather than overwriting** your existing hook config.

Status aggregates layer by layer from pane → tab → project (`error > ai-working > ai-idle > idle`). The moment a task flips from working to idle, four things fire, each independently toggleable:

- A bottom-right toast (only for inactive projects, deduplicated per project)
- A **DONE** badge in the project list
- Taskbar flashing (Windows) / Dock bouncing (macOS), only when the window is unfocused
- A notification sound (a built-in synthesized tone, or your own audio file)

### 📱 Watch your desktop AI from your phone, anywhere

This is probably the most distinctive thing Mini-Term does.

Fill in your relay address in the top-bar "Mobile" panel → save & connect → generate a pairing QR code. **Point your phone camera at it and the PWA opens and pairs itself.** From then on, while you're away you can:

- See **active AI sessions grouped by project**, with status lights synced live with the desktop
- Tap into any session for a **live conversation mirror** — AI replies rendered as Markdown, scroll up to page in older messages
- **Send commands** from the input box at the bottom — equivalent to typing it on the desktop keyboard and pressing Enter, with an immediate receipt
- **Start a brand-new session from your phone**: pick a project → pick an AI launcher, and the desktop opens a background tab and brings the agent up; once it's really running, your phone enters its mirror automatically
- **Rename a session** to something you'll recognize — the name shows up on the desktop tab too

The security boundaries were designed on purpose: pairing codes are single-use and valid for 10 minutes, pairing a new device replaces the old one, and "Reset pairing" revokes every credential instantly. **The relay forwards and never persists** — no message bodies stored, metadata-only logs (a subprocess-level automated test asserts zero file residue across the full flow). And an AI launcher's **command text never passes through the phone or the relay** — the phone references launchers by id and only ever sees the name.

> **Prerequisite**: the relay runs on **your own** server (1 vCPU / 1 GB is plenty, one Docker command to start, plus a domain pointed at it for TLS). That's deliberate — there is no third-party service in the middle. See the [deployment guide](docs/deploy-relay.md).

### 🧰 Turn your SSH connections into tools your AI can call

Saved SSH connections can be exposed as **MCP tools** to the Claude Code / Codex agents running in your terminal. Right-click a project → "Link SSH", tick the connections, and it's enabled for that project — with **visibility scoped to exactly the ones you ticked**.

The built-in `mt-ssh-mcp` sidecar (a stdio MCP server based on the official rmcp) provides four tools: `ssh_list_connections`, `ssh_exec`, `ssh_upload`, `ssh_download`. `ssh_exec` reuses your saved password / private-key auth, with timeouts, output capping, and an audit log. Uploads and downloads go over **SFTP in streamed chunks** — constant memory, large files work, no more base64-echoing through `ssh_exec` against an output cap.

The sidecar keeps an **in-process SSH session pool** (russh + tokio): the first call to a connection does one handshake + auth (seconds), and every command after that costs just one RTT. There's also a hard guard that refuses to ever transfer mini-term's own `config.json` (which holds every SSH credential in plaintext).

Enabling / disabling writes **idempotently** into Claude's `.mcp.json` and Codex's `.codex/config.toml` using named markers, so your hand-written config stays intact.

### 🌐 Remote directories as local projects — and WSL too

**SSH remote projects** — add a directory on a server as a project directly: the file tree lazy-loads over SFTP, the terminal connects via `ssh -t` and lands straight in the project directory, a one-click overlay reconnects after a drop, and the remote machine's Claude / Codex session history merges chronologically with full content viewing. Remote cache keys mix in the connection id, so identical paths on two different servers never cross-contaminate.

**WSL support** — `\\wsl$\<distro>\<path>` works as a project root. When the cwd is a WSL path, launching switches to `wsl.exe --cd` automatically, so `pwd` really lands inside WSL instead of `C:\Windows`. Windows can also read Claude / Codex session history from inside WSL distros directly (via UNC + registry enumeration, without spawning `wsl.exe`).

### 🪟 Multi-project · recursive splits · session history

- A **project sidebar** for multiple workspaces, with **up to 3 levels of nested groups**, drag-to-reorder, and drag-a-folder-from-Explorer to add
- **Arbitrarily nested horizontal / vertical splits**, drag to adjust ratios; tabs, splits, and window geometry all persist and restore on restart
- **Terminal caching** — switching projects, tabs, or panes never rebuilds the xterm instance, so nothing is lost; lazy startup creates a PTY only for the visible pane, so more history projects never means a slower launch
- **100k-line scrollback** with correct CSI 3J handling, so Codex transcript folding and `/clear` behave faithfully; the Windows build bundles a pinned official ConPTY runtime for consistent behavior across Windows versions
- **AI session history** — read local Claude / Codex records, right-click to copy the resume command, or read the full conversation right there (Markdown rendering + `Ctrl+F` search)
- **AI task markers** — every Enter inside a session drops a marker; `Ctrl+Shift+↑/↓` jumps between past submissions

### 🌿 Git integration + batch worktree management

A VS Code-style **Changes panel** (Staged / Changes / Untracked groups, per-file or bulk stage / discard, `Ctrl+Enter` to commit), side-by-side and inline diff views, cursor-paginated commit history, and a **hand-drawn SVG branch topology graph** (lane-based layout and coloring, merge commits as a filled dot inside a ring, with TOPOLOGICAL sorting in the backend revwalk so a rebase can't break the lines).

**Worktree management** is especially handy for running several agents in parallel: when the project root isn't a repo itself, it **scans downward for sub-repos** and groups them by main worktree, with checkable group headers (multi-select / select-all) so you can **create one worktree per checked repo in a single action** (the branch dropdown offers the intersection of all repos' branches). Any worktree can be turned into a project in one click — mounted under its parent as a sub-project — or just opened in a terminal.

![Git integration](docs/screenshots/git.png)

---

## And a pile of details tuned for working alongside AI

| | |
|---|---|
| **Long-text paste** | Clipboard text ≥10 lines or ≥2000 chars is spilled to a temp `.txt` and pasted as a quoted path — your AI tool never has to swallow a wall of text |
| **Image paste** | Screenshots in the clipboard are detected, saved as a temp PNG, and pasted as a path; handles non-standard formats like PinPix |
| **Remote-aware landing** | Both of the above remap in remote terminals: SSH projects upload over SFTP and paste the **remote** path; WSL projects rewrite `C:\...` into `/mnt/c/...` |
| **File drag & drop** | Drag from the file tree or Explorer onto the terminal to insert a quoted absolute path, landing in the exact split pane |
| **Global search** | `Ctrl+Shift+F` for filename or content search, substring or regex, streamed from the backend and cancellable anytime |
| **Per-project env vars** | Injected into the PTY child process per project, with strict POSIX validation and a second defensive filter on the Rust side; passes through to WSL via WSLENV |
| **Smart Ctrl+C/V** | Optional: copy when there's a selection, interrupt the program when there isn't; large Windows pastes are chunked so ConPTY doesn't drop lines |
| **Three themes + Blueprint skin** | Auto / Light / Dark (Warm Carbon), plus an optional sci-fi Blueprint skin; the native Windows title bar follows, with no light flash on startup |
| **Bilingual UI** | One click re-renders the whole interface in English / 中文, auto-detected from the system on first launch; in-house lightweight i18n, no extra runtime dependency |
| **Ligatures** | Composes `==` `=>` `!=` `->` glyphs (needs a calt-table font such as Fira Code / JetBrains Mono) |

---

## Tech stack

| Layer | Technology |
|---|---|
| Framework | Tauri v2 (Rust backend + system WebView — small installer, low resident memory) |
| Frontend | React 19 + TypeScript 5.8 + Tailwind CSS v4 + Vite 7 |
| Terminal | xterm.js v6 (WebGL, automatic Canvas fallback) |
| State / layout | Zustand single store · Allotment + recursive SplitNode tree |
| PTY / Git | portable-pty · git2 · notify + ignore |
| Mobile relay | axum + tokio WebSocket (`relay-server/`) · React + Vite PWA (`mobile/`) |
| Tests | **419 Rust tests** (381 desktop + 38 relay) plus 19 Node tests |

---

## Getting started

### Download

Grab the latest installer from [Releases](https://github.com/dreamlonglll/mini-term/releases).

> **Platform support**
> - **Windows** — the primary platform with guaranteed usability; all daily development and testing happens here
> - **macOS / Linux** — supported at the code level but **not well polished**; Issue reports are welcome

If macOS says "is damaged and can't be opened" on first launch, the file isn't actually corrupt — the Release artifact just isn't signed with an Apple Developer ID, so Gatekeeper rejects it. Drag the `.app` into `/Applications` and run this once:

```bash
xattr -cr /Applications/Mini-Term.app
```

### Build from source

Requires Node.js >= 20.19 (or >= 22.12), Rust >= 1.85, and the [Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/).

```bash
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term
npm install
npm run tauri dev      # dev (frontend + backend)
npm run tauri build    # release bundle
```

---

## More

- 📖 **[Full feature list](docs/features.md)** — every feature in detail, plus architecture overview and known limitations
- 📱 **[Relay deployment guide](docs/deploy-relay.md)** — the self-hosted relay behind the mobile features
- 🐛 **[Issues / PRs](https://github.com/dreamlonglll/mini-term/issues)** — external contributions are merged after functional verification and a security review

Learn AI, join the L site — [LinuxDO](https://linux.do/)

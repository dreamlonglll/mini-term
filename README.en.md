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
  <img src="https://img.shields.io/badge/version-1.2.2-blue" alt="version">
  <img src="https://img.shields.io/badge/platform-Windows-0078D4" alt="platform">
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux-experimental-lightgrey" alt="platform-experimental">
  <img src="https://img.shields.io/badge/GPUI-native-8A2BE2" alt="gpui">
  <img src="https://img.shields.io/badge/Rust-1.95%2B-dea584" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
</p>

<p align="center">
  <a href="https://github.com/dreamlonglll/mini-term/releases">Download</a> ·
  <a href="docs/features.md">Full feature list</a> ·
  <a href="docs/deploy-relay.md">Relay deployment</a>
</p>

**GPUI-native implementation**: Rust-native rendering, single process, no WebView2 dependency.

> The earlier Tauri + React implementation was removed from the repository and discontinued after v1.0.0-beta (old installers remain downloadable on past Releases; the source lives in git history).

---

## A familiar situation

You have four Claude Code sessions running, spread across three projects. **Which one finished? Which one is waiting on your approval?** Your system terminal won't tell you — you have to click through them one by one. And firing up VS Code or IDEA just for this trades a few hundred megabytes of RAM for a terminal window.

That is what Mini-Term is for. Status lights in the project list update live; the instant an AI task finishes you get a toast, a taskbar flash, and a sound. And when you're out of the house, your phone shows you the same live view — and lets you send the next instruction straight to it.

![Main UI](docs/screenshots/main.png)

---

## Things worth trying

### 🔔 Know the moment your AI is done

Not by guessing at process names — Mini-Term plugs directly into the **official Claude Code / Codex / Grok Build Hook APIs**. Events are reported in real time, which is both more accurate and faster than polling (process polling is kept as a fallback). Hooks are registered / unregistered **per CLI** in Settings, so using only one of them never writes config into the other two, and whatever is written merges with rather than overwrites your existing hook config.

Status aggregates layer by layer from pane → tab → project. The moment a task flips to finished, three things fire, each independently toggleable:

- A bottom-right toast
- A **DONE** badge in the project list
- Taskbar flashing (Windows) / Dock bouncing (macOS), only when the window is unfocused

### 📱 Watch your desktop AI from your phone, anywhere

Fill in your relay address in the top-bar "Mobile" panel → save & connect → generate a pairing QR code. **Point your phone camera at it and the PWA opens and pairs itself.** From then on, while you're away you can:

- See **active AI sessions grouped by project**, with status lights synced live with the desktop
- Tap into any session for a **live conversation mirror** — Markdown-rendered replies, scroll up to page in older messages
- **Send commands** from the input box at the bottom — equivalent to typing it on the desktop keyboard and pressing Enter, with an immediate receipt
- **Start a brand-new session from your phone**: pick a project → pick an AI launcher, and the desktop brings the agent up in a background tab

> **Prerequisite**: the relay runs on **your own** server (1 vCPU / 1 GB is plenty, one Docker command to start, plus a domain pointed at it for TLS). That's deliberate — there is no third-party service in the middle. See the [deployment guide](docs/deploy-relay.md).

### 📊 See what your AI spent this month, at a glance

The "Stats" panel in the top bar aggregates Claude Code / Codex / Grok **cost, calls, and sessions** across every dimension: daily / hourly trend charts, model and project rankings, top sessions, with ranges and scopes one click away.

> Cost computation follows the approach of the ccusage project — [ccusage/ccusage: npx ccusage](https://github.com/ccusage/ccusage)

### 🧰 Turn your SSH connections into tools your AI can call

Right-click a project → "Link SSH", tick the connections, and it's enabled for that project — with **visibility scoped to exactly the ones you ticked**. Enabling generates a `SKILL.md` for Claude and one for Codex (each embedding the CLI's absolute path and a random per-project capability token), so the agent loads the skill only when it needs it — no tool schema sits in the context window permanently, and since it's a plain command line, it composes with `grep`, pipes, and redirection.

### 🌐 Remote directories as local projects — and WSL too

- **SSH remote projects** — add a directory on a server as a project directly: the file tree lazy-loads over SFTP, the terminal connects via `ssh -t` and lands straight in the project directory, a one-click overlay reconnects after a drop, and the remote machine's Claude / Codex history is readable with full content. Remote cache keys mix in the connection id, so identical paths on two servers never cross-contaminate
- **Remote file management** — the remote file tree supports copy / paste / upload / download, dragging files in from Explorer uploads them, and the file panel header has shortcut buttons for uploading files / folders, pasting, and creating files / folders; name conflicts can be skipped, overwritten, or kept as copies, with the affected names listed in the dialog. Downloads land in the system Downloads folder by default (configurable in Settings); a remote directory picker lets you browse for the path when adding a remote project, and a context-menu action opens a remote directory in the terminal
- **WSL support** — `\\wsl$\<distro>\<path>` works as a project root, launching switches to `wsl.exe --cd` automatically so `pwd` really lands inside WSL instead of `C:\Windows`; Windows can also read Claude / Codex session history from inside WSL distros directly

### 🪟 Multi-project · recursive splits · session history

- A **project sidebar** for multiple workspaces, with up to 3 levels of nested groups, drag-to-reorder, and drag-a-folder-from-Explorer to add
- **Arbitrarily nested horizontal / vertical splits**, drag to adjust ratios; tabs, splits, and window geometry all persist and restore on restart
- **Project-level terminal panels** — an icon strip on the terminal area's right edge gives one project multiple **independent terminal workspaces**, each holding its own splits and tabs (one face for the AI session, another split for frontend + backend; click an icon to switch the whole face); buttons carry an AI progress light and a terminal-count badge, double-click to rename, everything restores on restart
- **New terminal, agent included** — all three "new terminal" entry points (tab-bar +, empty-state button, terminal panel) list your AI launchers alongside the shells (Claude / Codex preset, fully editable): pick one and a new pane opens with the launch command typed in, AI status detection attached; launchers share one config with the mobile "start a new session" flow (hidden on SSH remote projects — the early password prompts would eat the pre-typed command)
- **Transition animations** — directional pushes when switching tabs / panels, maximize expands from the pane's own cell and restore reverses it back; a single switch in Settings turns them all off
- **Drag panes to rearrange & maximize** — drag a tab into another group to merge, or onto a terminal-area edge to split off a new pane, with a live drop preview; double-click the tab bar's empty area to temporarily fill the terminal area, and content survives throughout
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
| **File workbench** | Local and remote files opened from the tree live in main-area tabs alongside the terminal, where you view, edit, and save them: tree-sitter syntax highlighting (30+ languages), find & replace, atomic `Ctrl+S` saves, external-change detection; remote files are read and written over SFTP, every save is checked against the loaded baseline, conflicts offer reload or force-overwrite, and any remote file can be downloaded |
| **Document preview** | Images actually render in the Markdown / HTML preview: relative paths resolve against the file's own directory, and remote images are fetched for real (10s timeout, 32MB cap, every other scheme refused). Markdown from remote files is sanitized before rendering: raw HTML shows as source, external images load only on click, and `file://`-style links degrade to plain text; remote HTML is source-view only. HTML previews also get an "Open in browser" button that resolves through the https protocol handler rather than the `.html` file association |
| **Global search** | `Ctrl+Shift+F` for filename or content search (a `/` in the query matches against the path), substring or regex, streamed from the backend and cancellable anytime |
| **Per-project env vars** | Injected into the PTY child process per project, with strict POSIX validation and a second defensive filter on the Rust side; passes through to WSL via WSLENV |
| **Smart Ctrl+C/V** | Optional: copy when there's a selection, interrupt the program when there isn't; large Windows pastes are chunked so ConPTY doesn't drop lines |
| **Dwell-to-copy selection** | Hold the mouse still after drag-selecting and the selection is copied with a "Copied" tip; dwell time configurable (0 = off) |
| **Alt+click to place the cursor** | Hold Alt (⌥ on macOS) and click anywhere on the command line to move the cursor there — arrow keys are synthesized from the column delta, same line only; cross-line clicks are ignored so the line editor's history recall never fires. Cell-accurate at shell prompts; Ink-style TUIs such as Claude CLI are best-effort |
| **Zero network requests at startup** | Native rendering, no web assets — startup makes no network request at all (the price table refreshes daily and falls back to its cache) |
| **Flood-proof UI** | PTY bytes feed the VT state machine on a background thread while the UI samples the grid per frame — single process, zero IPC, no intermediate buffer to pile up, so `cat`-ing a huge file can't drag the interface down |
| **External theme packs** | Dream Skin-compatible skins: import from a folder or a zip, sha256-verified against the manifest, hot-reloaded when you edit a file. A pack can ship its own background image, in which case the terminal goes translucent over that ambient layer. External references all pass the same gate (no `@import`; anything pointing outside the pack is rejected). Hit "More skins" to jump straight to the [`theme/`](theme/) gallery in this repo — pick one, download it, import it; to roll your own, the field reference lives in [`docs/theme-pack-example/`](docs/theme-pack-example/) |
| **Hover preview for project rows** | Hover for 250ms to pop up a preview of the project's running AI session terminal area |
| **Grouped settings panel** | A two-level sidebar: Terminal, Appearance, AI, System — every page fits on one screen instead of scrolling half a page to find a toggle |

---

## Tech stack

The whole application is **native Rust**:

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
| Tests | **1,677 Rust tests** (28 test targets) |

---

## Getting started

### Download

Grab the latest build from [Releases](https://github.com/dreamlonglll/mini-term/releases) — three platforms:

- **Windows x64 (primary platform)** — `Mini-Term_*_x64-setup.exe` installer (NSIS, per-user install without admin rights; upgrades in the same directory, and **uninstalls the old build first** instead of overwriting files)
- **macOS arm64** — `Mini-Term_*_aarch64.dmg`
- **Linux x64** — `Mini-Term_*_amd64.deb` or `Mini-Term_*_amd64.tar.gz`

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

## License

Released under the [MIT License](LICENSE).

Learn AI, join the L site — [LinuxDO](https://linux.do/)

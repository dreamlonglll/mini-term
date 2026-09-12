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
  <img src="https://img.shields.io/badge/version-1.2.11-blue" alt="version">
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

## Who it's for

- People juggling multiple projects
- Fans of the VS Code-style layout
- Terminal enthusiasts

![Main UI](docs/screenshots/main.png)

---

## A pile of details tuned for working alongside AI

| Feature | Description |
|:--|---|
| **AI event notifications** | Plugs directly into the **official Claude Code / Codex / Grok Build / oh-my-pi Hook APIs** (oh-my-pi through its in-process extension mechanism), with polling kept as a fallback.<br />Note: on Windows, Grok Build requires this app to be installed in a path **without spaces** for hooks to work properly |
| **External theme packs** | Dream Skin-compatible skins: import from a folder or a zip, sha256-verified against the manifest, hot-reloaded when you edit a file. A pack can ship its own background image, in which case the terminal goes translucent over that ambient layer. External references all pass the same gate (no `@import`; anything pointing outside the pack is rejected). Hit "More skins" to jump straight to the [`theme/`](theme/) gallery in this repo — pick one, download it, import it; to roll your own, the field reference lives in [`docs/theme-pack-example/`](docs/theme-pack-example/) |
| **Mobile support** | **Prerequisite**: the relay runs on **your own** server (1 vCPU / 1 GB is plenty, one Docker command to start, plus a domain pointed at it for TLS). See the [deployment guide](docs/deploy-relay.md). |
| **Cost statistics** | The "Stats" panel in the top bar aggregates Claude Code / Codex / Grok **cost, calls, and sessions** across every dimension: daily / hourly trend charts, model and project rankings, top sessions, with ranges and scopes one click away.<br />Cost computation follows the approach of the ccusage project — [ccusage/ccusage: npx ccusage](https://github.com/ccusage/ccusage) |
| **SSH support** | **SSH remote projects** — add a directory on a server as a project directly: the file tree lazy-loads over SFTP, the terminal connects via `ssh -t` and lands straight in the project directory, a one-click overlay reconnects after a drop, and the remote machine's Claude / Codex history is readable with full content. Remote cache keys mix in the connection id, so identical paths on two servers never cross-contaminate <br /><br />**WSL support** — `\\wsl$\<distro>\<path>` works as a project root, launching switches to `wsl.exe --cd` automatically so `pwd` really lands inside WSL instead of `C:\Windows`; Windows can also read Claude / Codex session history from inside WSL distros directly<br /><br />**Callable by agents** — a built-in Skill lets the AI run commands on your servers over SSH. Right-click a project → "Link SSH" and tick the connections to enable it per project |
| **Git integration** | A VS Code-style **Changes panel** (Staged / Changes / Untracked groups, per-file or bulk stage / discard, `Ctrl+Enter` to commit), plus **worktree management** (right-click a project → Manage Worktrees) |
| **Long-text paste** | Clipboard text ≥10 lines or ≥2000 chars is spilled to a temp `.txt` and pasted as a quoted path — your AI tool never has to swallow a wall of text |
| **Image paste** | Screenshots in the clipboard are detected, saved as a temp PNG, and pasted as a path; handles non-standard formats like PinPix |
| **Remote-aware landing** | Both of the above remap in remote terminals: SSH projects upload over SFTP and paste the **remote** path; WSL projects rewrite `C:\...` into `/mnt/c/...` |
| **File drag & drop** | Drag from the file tree or Explorer onto the terminal to insert a quoted absolute path, landing in the exact split pane |
| **Global search** | `Ctrl+Shift+F` for filename or content search (a `/` in the query matches against the path), substring or regex, streamed from the backend and cancellable anytime |
| **Per-project env vars** | Injected into the PTY child process per project, with strict POSIX validation and a second defensive filter on the Rust side; passes through to WSL via WSLENV |
| **Smart Ctrl+C/V** | On by default: copy when there's a selection, interrupt the program when there isn't, and `Ctrl+V` pastes directly; large Windows pastes are chunked so ConPTY doesn't drop lines |
| **Dwell-to-copy selection** | Hold the mouse still after drag-selecting and the selection is copied with a "Copied" tip; dwell time configurable (0 = off) |
| **Alt+click to place the cursor** | Hold Alt (⌥ on macOS) and click anywhere on the command line to move the cursor there — arrow keys are synthesized from the column delta, same line only; cross-line clicks are ignored so the line editor's history recall never fires. Cell-accurate at shell prompts; Ink-style TUIs such as Claude CLI are best-effort |
| **Zero network requests at startup** | Native rendering, no web assets — startup makes no network request at all (the price table refreshes daily and falls back to its cache) |
| **Flood-proof UI** | PTY bytes feed the VT state machine on a background thread while the UI samples the grid per frame — single process, zero IPC, no intermediate buffer to pile up, so `cat`-ing a huge file can't drag the interface down |
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
| Tests | **1,792 Rust tests** (29 test targets) |

---

## Getting started

### Download

Grab the latest build from [Releases](https://github.com/dreamlonglll/mini-term/releases) — three platforms:

- **Windows x64 (primary platform)** — `Mini-Term_*_x64-setup.exe` installer (NSIS, per-user install without admin rights; upgrades in the same directory, and **uninstalls the old build first** instead of overwriting files)
- **macOS arm64 (Apple Silicon)** — `Mini-Term_*_aarch64.dmg`
- **macOS x64 (Intel)** — `Mini-Term_*_x64.dmg`
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

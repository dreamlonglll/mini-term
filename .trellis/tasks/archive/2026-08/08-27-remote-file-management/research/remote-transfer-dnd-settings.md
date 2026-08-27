# Remote Transfer, Conflict, Drag-and-Drop, and Download Settings

## Existing transfer primitives

- `SftpHandle` currently exposes read/canonicalize/stat, recursive mkdir and exclusive small-file creation (`crates/mt-ssh/src/sftp.rs:74-244`).
- Single-file upload/download exist in `crates/mt-ssh/src/pool.rs:510-724`.
- Those functions open a new SFTP channel per file; upload holds the cached session lock for the transfer.
- Temporary names are fixed (`<target>.mt-sftp-partial`), so concurrent operations can collide.
- Overwrite uses remove + rename because standard SFTP rename cannot replace reliably; this creates a short missing-target window.
- `remote_ssh::upload_paste` is clipboard-specific and carries destination/.gitignore semantics, so it cannot be used as generic upload (`crates/mt-app/src/remote_ssh.rs:772-837`).

## Required transfer model

- Reuse one `SftpHandle` for a batch; add writable/remove/rename/streaming primitives.
- Use unique sibling temp and backup paths, exclusive creation and rollback-aware promotion.
- Do not follow symlinks; skip unsupported special entries with an explicit summary.
- Copy/paste remains in the current project and backend. Local↔remote uses explicit upload/download.
- Different SSH connections are not directly bridged in the MVP.

## Conflict semantics approved by the user

- Upload/download offer Skip, Overwrite and Keep Both.
- One selection applies to the remaining conflicts in the batch.
- Directory Overwrite means recursive merge: overwrite conflicting children and retain destination-only entries.
- Directory Keep Both creates a complete new sibling directory.
- Copy/paste conflicts default to Keep Both.
- Suggested Keep Both sequence: `name.ext`, `name copy.ext`, `name copy 2.ext`; preserve the final extension and handle dotfiles.

## Remote delete performance

- Remote delete does not exist today.
- Naive SFTP post-order deletion needs roughly `F + 2D` protocol operations for `F` non-directories and `D` directories, excluding guards.
- Reopening a channel for every file would add an unacceptable handshake multiplier.
- SFTP remains the authority for containment and verification. A capability-gated server-side delete may accelerate regular directories after SFTP parent/lstat validation; post-verify with SFTP and fall back when exec is unavailable.
- Existing safe shell quoting is in `crates/mt-pty/src/ssh.rs:83-95`; a generic exec implementation exists in `sidecars/src/ssh_service.rs:774-849` and should be moved, not copied.

## GPUI external file drop

- GPUI maps platform drops to `ExternalPaths`; existing examples are in `project_list.rs:1726-1884` and `terminal_area.rs:2454-2495`.
- `on_drag_move` fires for all registered elements, so handlers must check bounds.
- `on_drop` dispatches to the deepest hit element and stops propagation, enabling row-specific targets plus a root-background sibling.
- Target mapping: directory row → directory, file row → parent, background → project root.
- Only remote FileTree registers upload drops.

## File picker limitation

- `PathPromptOptions` cannot reliably select files and directories in one invocation; enabling directories selects directory mode.
- Menu should expose separate “Upload Files…” and “Upload Folder…” actions.
- External drag/drop naturally supports a mixed batch and uses the same upload pipeline.

## Download directory configuration

- The workspace already depends on `dirs` in mt-config/mt-app.
- Add `AppConfig.download_dir: Option<String>`; `None` means dynamic system default.
- Resolve with `dirs::download_dir()`, then `home/Downloads`; if neither is available, show a visible settings error.
- System settings should display the effective path and offer Choose / Restore Default.
- Download must revalidate/create the directory at operation time because it can be removed after configuration.


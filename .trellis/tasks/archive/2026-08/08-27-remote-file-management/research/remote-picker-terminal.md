# Remote Project Picker and Terminal CWD Research

## Remote project creation

- All three entries route to `remote_project::open`: project footer, group menu and first-run screen (`project_list.rs:929,2263`, `first_run.rs:110`).
- The current form has connection selection plus manual path/name only (`remote_project.rs:47-105,469-506`).
- Save runs `remote_ssh::validate_dir` on a background executor, then stores canonical POSIX path plus `ssh_connection_id` (`remote_project.rs:171-256`, `remote_ssh.rs:716-751`).
- Existing storage already supports the picker; no remote-project schema change is needed.

## Picker boundary

- The active-project FileTree cannot be embedded: it binds AppStore/current project and applies `.gitignore` plus fixed hidden directories.
- Add a lightweight overlay backed by a new unfiltered `browse_directory(connection, path)` service.
- One SFTP channel should canonicalize, verify and list directories.
- Use connection id + request id to reject stale results after rapid navigation.
- UI: Home, Root, Up, single-click enter, Choose Current Folder, loading/error/retry.
- Switching the selected connection resets the form path to `~` and clears old errors.
- Final Add always re-runs `validate_dir` after picker selection.

## Existing historical attempt

- A local Codex session on August 25, 2026 recorded an implementation of `remote_directory_picker.rs`, but no file or commit exists in the current Git history.
- Reuse the validated interaction conclusions only; do not assume recoverable source code.

## Open in terminal

- Correct API is `AppStore::new_terminal_with_cwd` (`store/panes.rs:48-82`).
- Directory target uses itself; file target uses its parent; project root passes `None`.
- The file-tree action must capture project id, not infer identity from a path shared by multiple servers.
- Current SSH branch computes cwd override but passes `project.path` to `prepare_remote_launch` (`store/panes.rs:684-709`).
- Fix it to use the resolved cwd; pane persistence and SSH reconnect already preserve `PaneState.cwd` (`tree.rs:96-105`, `persist.rs:34-108`, `store/ssh.rs:392-400`).
- Remote path parent calculation must use POSIX string helpers rather than host `Path::parent`.


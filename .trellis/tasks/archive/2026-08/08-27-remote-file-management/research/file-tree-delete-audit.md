# File Tree and Delete Audit

## Confirmed root causes

- `crates/mt-app/src/file_tree.rs:329-353` is the only local/remote read split.
- Rename, delete, create, default-open and reveal still invoke local services at `file_tree.rs:930-1123`.
- Remote create therefore sends a POSIX remote path to `mt_project::fs`; failure is hidden because create passes `failure=None` through `spawn_fs_op` (`file_tree.rs:864-908,1107-1119`).
- If the same absolute path exists locally, the current design can target local data instead of merely failing.
- `RevealInFolder` always calls local OS integration (`crates/mt-app/src/fs_ops.rs:80-105`), explaining the desktop/local-folder behavior.

## Context-menu targeting

- FileTree has no selected/hovered menu target state; each row captures its own `Row` and calls `stop_propagation` (`file_tree.rs:1616-1727`).
- The parent list owns the root menu (`file_tree.rs:1434-1478`).
- A row and its `flex_1` label consume the full horizontal width, so visual whitespace on a row still belongs to that entry.
- Robust fix: explicitly model Entry vs DirectoryBackground and put a `flex_1` background sibling after rows. Attach root menu/drop only to that sibling.

## Delete implementation and responsiveness

- Current chain: confirm → `spawn_fs_op` background executor → `mt_project::fs::delete_entry` → `remove_file/remove_dir_all` (`file_tree.rs:997-1039`, `mt-project/src/fs.rs:397-410`).
- It does not synchronously block GPUI rendering, but it occupies a background worker, has no busy/progress feedback, and may generate a watcher-event burst.
- Expanded descendants remain in `entries/watched/chain_owner` unless the project changes or directories are manually collapsed (`file_tree.rs:241-246,550-569`).
- Current tests cover only a tiny recursive directory (`mt-project/src/fs.rs:618-667`).

## Symlink safety defect

- `verify_under_project_root` canonicalizes the leaf (`mt-project/src/fs.rs:216-265`).
- `delete_entry` then removes the canonical target, so a symlink to a project-internal directory can delete the real directory and leave the link behind.
- A link to an external target cannot be removed because canonical containment rejects it.
- Required fix: canonicalize the parent, retain the raw basename, inspect the leaf with `symlink_metadata/lstat`, and remove the link itself.

## Async identity race

- `load_dir_with` captures an old connection but applies results without checking current project/connection (`file_tree.rs:325-404`).
- Entries are keyed only by `PathBuf`; two servers using `/home/u/app` can cross-contaminate after a project switch.
- Add project id, connection id and generation guards to every async read/write result.

## Adjacent remote-only gates

- Remote file click still invokes the local viewer (`file_tree.rs:581-585`).
- Remote header still exposes local search/editor actions (`file_tree.rs:1220-1296`).
- Historical React implementation hid these local-only actions; the GPUI migration added only remote list loading.

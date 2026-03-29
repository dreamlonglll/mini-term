# Terminal Layout Persistence Design

Persist terminal layout (tabs, splits, pane shell assignments) per project, and restore it on next open.

## Data Model

### Serialization Types (new)

Runtime `SplitNode` contains ephemeral data (`ptyId`, `status`, `id`). A parallel set of "saved" types strips these down to structure-only:

```typescript
interface SavedPane {
  shellName: string;
}

type SavedSplitNode =
  | { type: 'leaf'; pane: SavedPane }
  | { type: 'split'; direction: 'horizontal' | 'vertical'; children: SavedSplitNode[]; sizes: number[] };

interface SavedTab {
  customTitle?: string;
  splitLayout: SavedSplitNode;
}

interface SavedProjectLayout {
  tabs: SavedTab[];
  activeTabIndex: number;
}
```

Design decision: `SavedPane` only stores `shellName`, not `command`/`args`. On restore, the current `availableShells` config is used to resolve the full shell config. This means if a user renames or reconfigures a shell, restored panes will use the current config. This is intentional — the layout captures structure, not frozen shell configurations.

### ProjectConfig Extension

`ProjectConfig` gains an optional `savedLayout` field:

```typescript
interface ProjectConfig {
  id: string;
  name: string;
  path: string;
  savedLayout?: SavedProjectLayout;
}
```

### Rust Structs

All new structs use `#[serde(rename_all = "camelCase")]`. The new field on `ProjectConfig` uses `#[serde(default)]` for backward compatibility with existing `config.json` files that lack this field.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPane {
    pub shell_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum SavedSplitNode {
    Leaf { pane: SavedPane },
    Split {
        direction: String,
        children: Vec<SavedSplitNode>,
        sizes: Vec<f64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedTab {
    #[serde(default)]
    pub custom_title: Option<String>,
    pub split_layout: SavedSplitNode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedProjectLayout {
    pub tabs: Vec<SavedTab>,
    pub active_tab_index: usize,
}

// ProjectConfig updated:
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub saved_layout: Option<SavedProjectLayout>,
}
```

## Serialization (Save)

A pure function `serializeLayout(ps: ProjectState): SavedProjectLayout` in `store.ts`:

1. Iterates `ps.tabs`, for each tab recursively walks the `SplitNode` tree
2. At each leaf, extracts `shellName` only (drops `id`, `ptyId`, `status`)
3. At each split, preserves `direction`, `children`, `sizes`
4. Records `activeTabIndex = tabs.findIndex(t => t.id === ps.activeTabId)`

## Deserialization (Restore)

An async function `restoreLayout(projectId, savedLayout, projectPath, config)` in `store.ts`:

1. Iterates `savedLayout.tabs`, for each `SavedTab` recursively walks `SavedSplitNode`
2. At each leaf:
   - Finds shell config by matching `shellName` against `config.availableShells[].name`
   - Falls back to `config.defaultShell` if not found
   - If no shell can be resolved at all (empty `availableShells` and no `defaultShell` match), skip this pane (return `null`)
   - Calls `invoke('create_pty', { shell, args, cwd: projectPath })` to get a `ptyId`
   - On PTY creation failure, return `null`
   - On success, assembles `PaneState { id: genId(), shellName, status: 'idle', ptyId }`
3. Assembles `TerminalTab { id: genId(), splitLayout, status: 'idle', customTitle }`
4. Sets `activeTabId = tabs[savedLayout.activeTabIndex]?.id ?? tabs[0]?.id ?? ''`

### Error Handling During Tree Construction

Errors are handled inline during the recursive tree build, not post-hoc:

- Leaf node PTY creation fails → recursive function returns `null`
- Parent split node filters out `null` children, recalculates `sizes` as equal proportions
- If a split has only one child remaining → unwrap to that child
- If a split has zero children → return `null` (propagates up)
- Tab with `null` root layout → skip the tab
- All tabs skipped → degrade to empty tabs (as if no savedLayout)
- Silent degradation, no user-facing error

## Save Triggers

Reuse existing `save_config` + 500ms debounce. A new `saveLayoutToConfig()` method:

1. Calls `serializeLayout()` on current project's `ProjectState`
2. Writes result into `ProjectConfig.savedLayout`
3. Calls `invoke('save_config', { config })` (debounced)

Trigger points:

| Operation | Location | Hook |
|-----------|----------|------|
| New tab | `TerminalArea.tsx` `handleNewTab` | After addTab |
| Split pane | `TerminalArea.tsx` `handleSplitPane` | After insertSplit |
| Close pane | `TerminalArea.tsx` `handleClosePane` | After removePane |
| Close tab | `TerminalArea.tsx` `handleCloseTab` | After tab removal |
| Drag tab to split | `TerminalArea.tsx` `handleTabDrop` | After layout update |
| Resize splits | `SplitLayout.tsx` Allotment `onChange` | See below |
| Switch project | `App.tsx` / store `setActiveProject` | Before switching |
| App closing | `window` `beforeunload` | Flush immediately (no debounce) |

### Allotment Resize Sizes Propagation

Current `SplitLayout.tsx` passes `node.sizes` as `defaultSizes` to Allotment but has no `onChange` handler. To capture resize changes:

- Add `onChange` prop to `SplitLayout`'s recursive rendering
- `SplitLayout` accepts a new callback prop: `onLayoutChange(updatedNode: SplitNode)`
- When Allotment `onChange` fires, clone the current `SplitNode` with the new sizes array, call `onLayoutChange` with the updated tree
- `TerminalArea` receives this callback, calls `updateTabLayout` to persist the new sizes into the store, then triggers `saveLayoutToConfig`

This propagation works because each recursive `SplitLayout` instance knows its own node and can reconstruct the updated subtree.

## Restore Timing

In `App.tsx` initialization `useEffect`, after `load_config`:

1. First, synchronously initialize all project states with empty tabs (current behavior) — ensures UI renders immediately
2. Then, for each project with a `savedLayout`, kick off `restoreLayout()` asynchronously
3. Use `Promise.all` to restore all projects in parallel
4. Each `restoreLayout` updates the store once complete, replacing the empty tabs

This avoids blocking UI rendering. The user may see empty tabs briefly before restoration completes, which is acceptable given the PTY creation latency.

## Files Changed

| File | Change |
|------|--------|
| `src/types.ts` | Add `SavedPane`, `SavedSplitNode`, `SavedTab`, `SavedProjectLayout`; extend `ProjectConfig` |
| `src-tauri/src/config.rs` | Add Rust structs with serde annotations; extend `ProjectConfig` with `#[serde(default)]` |
| `src/store.ts` | Add `serializeLayout()`, `restoreLayout()`, `saveLayoutToConfig()` |
| `src/App.tsx` | Call `restoreLayout` during init; add `beforeunload` handler |
| `src/components/TerminalArea.tsx` | Call `saveLayoutToConfig` after layout mutations |
| `src/components/SplitLayout.tsx` | Add Allotment `onChange` + `onLayoutChange` callback prop |

## Not Changed

- No new Tauri commands (reuses `save_config` / `load_config` / `create_pty`)
- No new storage files (reuses `config.json`)
- No changes to PTY lifecycle management

# State Management

> How state is managed in this project.

---

## Overview

Global state lives in a single Zustand store (`useAppStore` in `src/store.ts`).
Its `config: AppConfig` field is the persisted configuration: loaded from
`config.json` on startup via the `load_config` Tauri command, and written back
via `save_config` after any change. Components subscribe with selectors
(`useAppStore((s) => s.xxx)`); non-component code reads the latest value with
`useAppStore.getState()`.

---

## State Categories

<!-- Local state, global state, server state, URL state -->

(To be filled by the team)

---

## When to Use Global State

<!-- Criteria for promoting state to global -->

(To be filled by the team)

---

## Server State

<!-- How server data is cached and synchronized -->

(To be filled by the team)

---

## Common Mistakes

<!-- State management mistakes your team has made -->

(To be filled by the team)

---

## Extending AppConfig

### Convention: Adding a field to the AppConfig schema

**What**: Adding a field to the global `AppConfig` requires synchronized edits in
**four** places across two languages. Missing any one causes a compile error or a
silent inconsistency.

**Why**: `AppConfig` is the persistence contract shared between the Rust backend
and the frontend (`config.json`), carried by the `load_config` / `save_config`
Tauri commands. The four spots live in different files and languages, so a miss is
not caught by reviewing any single file.

**The four spots**:

1. `src-tauri/src/config.rs` — add the field to the `AppConfig` struct, with
   `#[serde(default ...)]` so old `config.json` files still deserialize
2. `src-tauri/src/config.rs` — add the field's default to `impl Default for
   AppConfig` (Rust will not compile without it)
3. `src/types.ts` — add the same field to the frontend `AppConfig` interface
4. `src/store.ts` — add the field to the store's initial `config` literal
   (TypeScript will not compile — the literal would be missing a required property)

**Naming**: Rust uses snake_case, the frontend uses camelCase. The struct's
`#[serde(rename_all = "camelCase")]` maps them automatically — no per-field
`#[serde(rename)]` needed.

**Example** (the `smartCopyPaste` field):

```rust
// config.rs — struct
#[serde(default)]
pub smart_copy_paste: bool,

// config.rs — impl Default for AppConfig
smart_copy_paste: false,
```

```typescript
// types.ts — AppConfig interface
smartCopyPaste: boolean;

// store.ts — initial config literal
smartCopyPaste: false,
```

**Optional fields**: if the field is semantically nullable, use `Option<T>` +
`#[serde(default, skip_serializing_if = "Option::is_none")]` on the Rust side and
`field?: T` on the frontend.

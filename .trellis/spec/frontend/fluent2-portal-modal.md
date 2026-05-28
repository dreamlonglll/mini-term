# Fluent 2 + backdrop-filter:Modal / iframe Portal Convention

> Under the **Fluent 2** skin, `[data-panel]` containers apply
> `backdrop-filter: blur(24px) saturate(140%)` for the acrylic glass effect.
> Per CSS spec, this turns the panel into a **containing block** for
> descendant `position: fixed` elements, which **pulls full-screen modals
> and iframes inside the panel boundary** instead of attaching them to
> the viewport. Always render such overlays through `createPortal` into
> `document.body`.

## Scope / Trigger

Any new modal / popover / full-screen iframe / overlay that:

- Uses `position: fixed inset-0` (or similar viewport-locked positioning)
- May render inside any subtree of `ProjectList`, `FileTree`, `GitHistory`,
  or any other ancestor styled with `[data-panel]` /
  `backdrop-filter` / `transform` / `filter` / `perspective` /
  `will-change` / `contain`.

**Reference fixes**:

- Commit `e7316e5` (v0.4.20) migrated **7 modals** after Fluent 2 acrylic
  shipped:
  `SessionViewerModal` / `CommitDiffModal` / `FileViewerModal` /
  `DiffModal` / `ProjectEnvVarsModal` / `SshAssocModal` /
  `ProjectList` inline delete-confirm
- Task `05-28-embed-cc-connect-panel` PR3 extended the same fix to
  `CcConnectDashboard` (keep-alive iframe)

## Contracts (CSS spec the issue derives from)

CSS specifies that any ancestor with one of the following non-default
properties becomes the **containing block** for descendant
`position: fixed`:

- `backdrop-filter` (any non-`none` value)
- `transform`, `filter`, `perspective` (any non-`none` value)
- `will-change: transform | filter | perspective | backdrop-filter`
- `contain: paint | layout | strict | content`

When this happens, `fixed inset-0` **no longer attaches to the viewport**;
it attaches to the nearest such ancestor — typically a sidebar panel —
producing an overlay that is **clipped inside the panel** and looks
"stuffed" rather than fullscreen-centered.

## When Portal Is Required

| Component lives under | Portal required? |
|---|---|
| `App.tsx` top level (siblings of `Allotment`) | No(no ancestor triggers containing block) |
| `SettingsModal` / `SearchModal`(already top-level under App) | No |
| Inside `ProjectList` / `FileTree` / `GitHistory` / any `[data-panel]` subtree | **Yes** |
| Anywhere new acrylic / blur / transform might be added later | **Yes**(future-proof) |

**Recommendation**: even if your overlay currently lives at `App.tsx`
top level, prefer `createPortal` unconditionally. The cost is one extra
import; the benefit is immunity to future ancestor styling.

## Pattern

### Standard modal (unmount on close)

```tsx
import { createPortal } from 'react-dom';

export function MyModal({ open, onClose, ...rest }: Props) {
  if (!open) return null;
  const node = (
    <div className="fixed inset-0 z-50 bg-black/40 flex items-center justify-center">
      <div className="bg-[var(--bg-surface)] p-4 rounded">
        ...
        <button onClick={onClose}>关闭</button>
      </div>
    </div>
  );
  return createPortal(node, document.body);
}
```

### Keep-alive iframe (avoid re-login on each open)

```tsx
export function CcConnectDashboard({ open, onClose, url }: Props) {
  return createPortal(
    <div
      style={{ display: open ? 'flex' : 'none' }}
      className="fixed inset-0 z-50 bg-black/40"
    >
      <iframe src={url} className="flex-1" />
      <button onClick={onClose}>关闭</button>
    </div>,
    document.body,
  );
}
```

Do **not** `return null` when closed — the iframe DOM and its login
session would be discarded, forcing a `?token=` re-login next time. Use
`display: none` instead.

## Validation & Error Matrix

| Symptom | Cause | Fix |
|---|---|---|
| Modal renders inside a sidebar panel instead of centered on viewport | Ancestor has `backdrop-filter`(Fluent 2 `[data-panel]`)or other containing-block-trigger | `createPortal(node, document.body)` |
| Modal correct in default / blueprint skin but clipped in Fluent 2 | Containing block only forms when `backdrop-filter` is non-`none`; default skin leaves it `none` | `createPortal` regardless of skin |
| `useEffect` cleanup runs more often than expected after portal change | Refs into portaled subtree lost on unmount | `useRef` + null-check, or place ref logic on root of portaled node |
| iframe re-issues `login?token=` fetch every time the panel opens | Used `if (!open) return null` instead of `display: none` | Switch to keep-alive pattern |

## Good / Base / Bad Cases

- **Good**:every new full-screen overlay uses `createPortal(node, document.body)` regardless of current skin and ancestor styling. Future skin changes never re-break it.
- **Base**:only Fluent 2-conditional portal (`if (skin === 'fluent2') createPortal else inline`). Works today but fragile when more skins / panels add containing-block-trigger properties.
- **Bad**:inline `<div className="fixed inset-0">` rendered inside `ProjectList` subtree. Works in default skin, breaks under Fluent 2, breaks again when any ancestor gains `transform` / `filter` / `will-change`.

## Wrong vs Correct

### Wrong

```tsx
// Inline render inside a panel subtree
function ProjectList() {
  const [dashOpen, setDashOpen] = useState(false);
  return (
    <div data-panel>  {/* Fluent 2 backdrop-filter triggers here */}
      <ul>...</ul>
      {dashOpen && (
        <div className="fixed inset-0 z-50 bg-black/40">  {/* ❌ trapped inside panel */}
          <iframe src={url} />
        </div>
      )}
    </div>
  );
}
```

### Correct

```tsx
// Separate component using createPortal
function ProjectList() {
  const [dashOpen, setDashOpen] = useState(false);
  return (
    <>
      <div data-panel>
        <ul>...</ul>
      </div>
      <CcConnectDashboard open={dashOpen} onClose={() => setDashOpen(false)} url={url} />
    </>
  );
}

function CcConnectDashboard({ open, onClose, url }: Props) {
  return createPortal(
    <div
      style={{ display: open ? 'flex' : 'none' }}
      className="fixed inset-0 z-50 bg-black/40"
    >
      <iframe src={url} className="flex-1" />
      <button onClick={onClose}>关闭</button>
    </div>,
    document.body,  // ✓ escapes any [data-panel] containing block
  );
}
```

## Tests Required

- **Visual smoke under Fluent 2 skin**: open the modal / iframe from a
  deeply nested location (e.g. right-click a `ProjectList` item) — the
  overlay must cover the entire viewport, not be clipped inside the panel.
- **Visual smoke under non-Fluent 2 skins**: same behavior (centered on
  viewport, no regression).
- **Keep-alive check** (iframe only): open → close → reopen → iframe
  `contentWindow` should retain previous session; no additional
  `login?token=` request in the network tab.
- **Stacked overlays**: opening another portaled modal on top of the
  iframe should still respect z-index ordering.

## Related

- commit `e7316e5` — 7-modal migration that established this convention
- `src/components/CcConnectDashboard.tsx` — keep-alive iframe reference implementation
- `src/fluent2.css` — `[data-panel] { backdrop-filter: blur(24px) saturate(140%); }` (the trigger)
- [../backend/cc-connect-integration.md](../backend/cc-connect-integration.md) — dashboard iframe URL spec
- MDN: [Containing block — `backdrop-filter`](https://developer.mozilla.org/en-US/docs/Web/CSS/Containing_block#identifying_the_containing_block)

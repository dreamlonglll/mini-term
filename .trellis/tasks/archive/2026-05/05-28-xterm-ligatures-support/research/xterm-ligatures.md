# Research: xterm.js Font Ligatures Support (Tauri 2 + WebGL)

- **Query**: How to add font ligatures support to a Tauri 2 desktop app using xterm.js v6 + `@xterm/addon-webgl@0.19`
- **Scope**: external (npm + GitHub upstream)
- **Date**: 2026-05-28
- **Target stack**: `@xterm/xterm@^6.0.0` + `@xterm/addon-webgl@^0.19.0` + Tauri 2 (Windows WebView2 / macOS WKWebView / Linux WebKitGTK)
- **Addon under review**: `@xterm/addon-ligatures@0.10.0` (published 2026)

> Important: the **README on npm/GitHub is stale**. It still claims "default canvas renderer" and depends on `font-finder` (Node). The **actual published `lib/addon-ligatures.mjs` and master-branch source no longer use `font-finder` at runtime** — they use the browser **Local Font Access API** (`navigator.fonts.query()` / `window.queryLocalFonts()`). Treat the README as misleading and trust the source.

---

## 1. Renderer Compatibility (the actual answer)

**`@xterm/addon-ligatures@0.10.0` works with the WebGL renderer.** It is the officially-supported combination per the typings and the demo.

Evidence:

- **Typings explicitly call out WebGL** (`addons/addon-ligatures/typings/addon-ligatures.d.ts`):
  > "Activates the addon. Note that if webgl is also being used, that addon **should be reactivated after ligatures is activated** in order to apply `fontFeatureSettings` to the texture atlas."
  Source: https://github.com/xtermjs/xterm.js/blob/master/addons/addon-ligatures/typings/addon-ligatures.d.ts
- **The official xterm.js demo wires `LigaturesAddon` + `WebglAddon` together** and explicitly recreates `WebglAddon` whenever ligatures are toggled (see `demo/client/client.ts` around the `if (name === 'ligatures')` branch).
  Source: https://github.com/xtermjs/xterm.js/blob/master/demo/client/client.ts
- The closed bug `#5205` ("Ligatures don't clear cursor background color on webgl renderer") proves the project actively tests and fixes the WebGL+ligatures combination.
  https://github.com/xtermjs/xterm.js/issues/5205
- The DOM renderer (built into core, used when no WebGL addon is loaded) **also** supports ligatures since `#5207` fix — the addon now writes `font-feature-settings: "calt" on` to `terminal.element.style`.
  https://github.com/xtermjs/xterm.js/issues/5207
- The legacy **Canvas renderer addon (`@xterm/addon-canvas`) is effectively abandoned** — last publish 2024-04-05 (v0.7.0), source removed from xterm.js master (see issue `#4779`). It still exists on npm but is no longer maintained.
  https://github.com/xtermjs/xterm.js/issues/4779

**Recommendation = WebGL + Ligatures (works, no need to switch renderer).** The README's "canvas renderer" wording predates the rewrite.

### Known WebGL+ligatures glitches (still open or recently fixed)

| Issue | Status | Symptom |
|---|---|---|
| [`#3303`](https://github.com/xtermjs/xterm.js/issues/3303) | **open** | Ligatures leave paint remnants on WebGL when typing `=`, `==`, `===` in fast succession (zsh syntax-highlight scenario). Idle since 2023. |
| [`#4000`](https://github.com/xtermjs/xterm.js/issues/4000) | **open** | Text **selection rectangle** doesn't render correctly across ligatures on WebGL only. |
| [`#4362`](https://github.com/xtermjs/xterm.js/issues/4362) | **open** | Fira Code's extremely long `=====…` ligature can trigger an infinite loop in WebGL's texture atlas allocation. |
| [`#5455`](https://github.com/xtermjs/xterm.js/issues/5455) | **open** | Changing `font-feature-settings` at runtime may not apply (VS Code #255152). Workaround: dispose & recreate the WebGL addon. |
| [`#5205`](https://github.com/xtermjs/xterm.js/issues/5205) | closed | Cursor background not cleared after a ligature on WebGL. |
| [`#3288`](https://github.com/xtermjs/xterm.js/issues/3288) | closed | Type-ahead in VS Code stored wrong cell colors with WebGL+ligatures. |

Net: WebGL+ligatures is the supported path but ships with **known cosmetic bugs** around selection rectangles and rapid edits. None of these are showstoppers for typical terminal use.

---

## 2. Browser / Webview Compatibility

### What the addon actually needs at runtime

The published `0.10.0` bundle (`package/lib/addon-ligatures.mjs`, 204 KB) calls these browser APIs to discover system fonts (verified by unpacking the tarball and grepping the bundle):

```js
// modern path: Window.queryLocalFonts (Chrome 103+, Edge 103+)
if ('queryLocalFonts' in window) {
  const fontsIterator = await window.queryLocalFonts();
  // ...
}
// legacy path: navigator.fonts.query() (older incarnation of same API)
else if ('fonts' in navigator) {
  await navigator.permissions.request?.({ name: 'local-fonts' });
  const fontsIterator = await navigator.fonts.query();
  // ...
}
```

- **No Node `fs`, no `font-finder`, no `font-ligatures` import at runtime.** The `font-finder@1.1.0` in `dependencies` of the published `package.json` is dead weight — npm install pulls it in but the bundle doesn't import it.
- **No `eval` and no WebAssembly** — `grep -E "(new Function|eval\(|WebAssembly)"` against the bundle returns nothing. So **no `'unsafe-eval'` CSP needed**.
- Dependencies on master are now just `lru-cache@^11` and `opentype.js@^2` (font-ligatures was merged into the addon in commit `363fc84` on 2026-01-03).
  https://github.com/xtermjs/xterm.js/blob/master/addons/addon-ligatures/package.json

### Local Font Access API browser support

Source: MDN browser-compat-data (`api/Window.json`, `queryLocalFonts`).

| Engine | Support | Tauri Webview on this OS |
|---|---|---|
| Chrome / Chromium | **103+** (since 2022-06) | — |
| Edge | **103+** (mirrors Chromium) | **Windows WebView2** — supported |
| Firefox | **No** (no implementation) | — |
| Safari / WebKit | **No** | **macOS WKWebView, iOS WKWebView** — NOT supported |
| Android WebView | mirrors Chromium **but Android Chrome itself is `false`** (https://crbug.com/40840834) | n/a |

**Practical consequences for mini-term (Tauri 2):**

| OS | Webview | `queryLocalFonts` works? | Result |
|---|---|---|---|
| Windows 10/11 | WebView2 (Chromium 103+) | **Yes** | Full ligature parsing from `.ttf`/`.otf` on disk. |
| macOS | WKWebView (Safari/WebKit) | **No** | Addon silently falls back to the hard-coded `fallbackLigatures` list (60 sequences). No font-specific shaping. |
| Linux | WebKitGTK | **No** (same engine as Safari) | Same fallback path as macOS. |

In other words: **on Windows you get real ligatures from the loaded font; on macOS/Linux you get a generic "calt" fallback set** drawn from Iosevka's default ligation: `<--`, `->`, `=>`, `===`, `!==`, `>=`, `<=`, `==`, `!=`, `::`, etc. (full list in `LigaturesAddon.ts` constructor and in the [Iosevka README](https://github.com/be5invis/Iosevka?tab=readme-ov-file#ligations)). This is still visibly nicer than no ligatures at all and is the same set VS Code uses as its built-in fallback.

### Permissions / Secure Context / Permissions-Policy

Per the WICG spec (https://wicg.github.io/local-font-access/):

- "Secure context" required — Tauri serves over `tauri://` / `https://tauri.localhost`, which **is** treated as a secure context, so this is satisfied automatically.
- **User permission required.** First call to `queryLocalFonts()` prompts the user. **WebView2 inherits Edge/Chromium permission UI** — the addon's first run will produce a permission prompt the first time it tries to access fonts. If the user denies, the promise rejects and the addon falls back to its `fallbackLigatures`.
- **Permissions-Policy header `local-fonts`** can disable this. Tauri does not set any restrictive Permissions-Policy by default, so this is moot.
- **CSP**: no impact. Local Font Access is gated by Permissions, not by CSP. The project's current CSP (`default-src 'self'; script-src 'self'; ...` in `src-tauri/tauri.conf.json`) is sufficient — **no changes needed**.

There is one Tauri-specific gotcha worth flagging: Tauri/Wry does not currently expose a Rust-side hook to **pre-grant** the `local-fonts` permission (see open PR https://github.com/tauri-apps/wry/pull/1654 for general permission handler support). On first run the user will see a Chromium permission bubble. This is **unclear** whether WebView2 silently auto-approves or shows UI — no public confirmation found from a Tauri user yet.

---

## 3. xterm.js v6 Compatibility

- `@xterm/addon-ligatures@0.10.0`'s `package.json` declares **no peerDependencies** (verified from npm metadata).
- `engines.node: ">8.0.0"` is leftover and irrelevant — there is no required Node runtime.
- The official typings import `from '@xterm/xterm'` and the bundled `.mjs` re-exports `LigaturesAddon`. It uses public API only: `registerCharacterJoiner`, `deregisterCharacterJoiner`, `terminal.element.style`, `terminal.options.fontFamily`. All of these are stable across xterm.js v5 → v6.
- The latest npm publish of `addon-ligatures@0.10.0` happened **after** xterm.js v6 shipped (uses Node 22 in the package metadata) and the latest commit referenced is `f447274f` from a release that already targets `@xterm/xterm@^6.0.0` ecosystem.
- **No open GitHub issues** mention v6-specific breakage of the ligatures addon (searched issues across xtermjs/xterm.js).

**Verdict: compatible with `@xterm/xterm@^6.0.0` without caveats.**

---

## 4. Performance and Behavior

### Performance impact

There is **no published benchmark** of "WebGL + ligatures" vs "WebGL alone", but qualitative behavior is known from the source:

- Adding the LigaturesAddon registers a single **character joiner** callback (`term.registerCharacterJoiner`) that runs once per rendered cell range. The callback returns an array of `[startCol, endCol]` ranges. Per-call cost is O(line length) text scan against the LRU cache (`lru-cache@^11`, `CACHE_SIZE = 100000`) which is cheap.
- The first call per `fontFamily` triggers an **async** font fetch via Local Font Access + opentype.js parse. This is one-time per font (cached at module scope in `fontsPromise` and per-font in `font.ts`).
- After that, ligature lookup is hot-path cached. Expected steady-state overhead is **single-digit percent at most**, dominated by the texture-atlas re-shaping cost on WebGL (long ligatures take more atlas space; see `#4362`).
- The **font-resolution cache is module-scoped** (`let fontsPromise`). It is **never invalidated**. So if the user changes `terminalFontFamily`, the addon does fire the loader for the new family (logic at `font.ts:enableLigatures` checks `currentFontName !== termFont`), and re-runs `queryLocalFonts` only if the font wasn't already in the global cache from the first call. On Windows the first `queryLocalFonts` call enumerates **all** installed fonts in one go and caches them.
- The atlas-rebuild bug `#5455` means **`fontFeatureSettings` changes** (not `fontFamily`) may need an explicit `WebglAddon` recreate to take effect.

### Switching to DOM renderer instead?

If WebGL were unworkable, the DOM renderer is the only built-in fallback (canvas addon is dead). The DOM renderer is **significantly slower than WebGL** — historically 5-10× slower on text throughput for large dumps (per xterm.js maintainer comments in `#4779`), which is why the project moved to WebGL by default. For a project like mini-term that explicitly degrades from WebGL → Canvas in `terminalCache.ts:activateWebgl`, the right answer is to **stay on WebGL and just add ligatures on top**; DOM should remain a hard-fallback only.

### Runtime fontFamily changes

`terminalCache.ts` already mutates `term.options.fontFamily` on user font change. With ligatures loaded, the addon's character-joiner closure is keyed on `currentFontName` and will:
1. Mark loading state, return fallback ranges for the next frame.
2. Async-load the new font's ligature table via `queryLocalFonts({ postscriptNames: [...] })` → `blob()` → opentype.js parse → store in closure-local `font` variable.
3. On success, call `term.refresh(0, term.rows - 1)`.

Steps 1-3 are correct and require no app-side intervention. **No need to recreate the LigaturesAddon when only `fontFamily` changes.**

---

## 5. Alternative Approaches (to keep WebGL AND get ligatures)

There is no real "alternative" needed — **WebGL + `@xterm/addon-ligatures` is the supported combination**. That said, for completeness:

| Approach | Status | Notes |
|---|---|---|
| `@xterm/addon-ligatures@0.10.0` on top of `@xterm/addon-webgl@0.19` | **Officially supported** (typings, demo, active bug-fixing) | Recommended path. |
| `@xterm/addon-canvas@0.7.0` + addon-ligatures | Works but **canvas addon is abandoned** since 2024-04, removed from xterm.js master tree (`#4779`). | Avoid. |
| DOM renderer (no webgl/canvas addon) + addon-ligatures | Works since `#5207` fix. | Slow but the only renderer if both WebGL and Canvas are unavailable. Useful as a last-resort fallback. |
| `xterm-addon-ligatures-wasm` or community forks | **Does not exist on npm.** `npm search xterm+ligatures` returns only the official `@xterm/addon-ligatures` and unrelated packages. | n/a |
| Shape ligatures in WebGL atlas directly (no addon) | **Not implemented.** No issue or PR proposing it. The current architecture pushes shaping out to the character-joiner callback because OpenType GSUB is too heavy for the renderer to run inline. | n/a |
| Use CSS `font-feature-settings: "liga" on, "calt" on` without the addon | Partial. Without the character joiner, xterm.js still measures and renders glyphs **per-cell**, so multi-glyph ligatures break into individual chars. The CSS alone is not enough — the addon's `registerCharacterJoiner` is what tells xterm.js to merge cells. | Don't do this alone. |

**Conclusion: there is no maintained alternative.** Stay on the official addon.

---

## 6. Concrete Integration Sketch

Below is the minimum code change for a project that already has `WebglAddon` loaded (which mini-term does in `src/utils/terminalCache.ts:activateWebgl`).

### Install

```bash
npm install --save @xterm/addon-ligatures
```

This pulls in the addon plus three runtime deps the **published bundle** doesn't actually use (`font-finder`, `font-ligatures` carry-over from old package.json) plus `lru-cache` and `opentype.js`. Bundle size impact: the `.mjs` is ~200 KB minified — non-trivial. Consider lazy-loading if cold-start matters.

### Minimum addon load (no toggling)

```ts
import { Terminal } from '@xterm/xterm';
import { WebglAddon } from '@xterm/addon-webgl';
import { LigaturesAddon } from '@xterm/addon-ligatures';

const term = new Terminal({ fontFamily: '"Fira Code", "JetBrains Mono", monospace' });
term.open(container);                       // 1. open first (required, addon throws otherwise)

const ligatures = new LigaturesAddon();     // 2. options optional
term.loadAddon(ligatures);                  // 3. load ligatures BEFORE webgl per official ordering note

const webgl = new WebglAddon();
term.loadAddon(webgl);                      // 4. webgl reads the now-set font-feature-settings into its atlas
```

Order rationale (from the typings doc-comment): "if webgl is also being used, that addon should be **reactivated after ligatures is activated** in order to apply `fontFeatureSettings` to the texture atlas." So either load ligatures first, OR dispose+re-create webgl after loading ligatures (which is what the demo does for runtime toggling).

### Runtime toggle on/off (the pattern the official demo uses)

```ts
function enableLigatures(term: Terminal, webgl: WebglAddon): { ligatures: LigaturesAddon; webgl: WebglAddon } {
  const ligatures = new LigaturesAddon();
  term.loadAddon(ligatures);
  // recreate webgl so its texture atlas picks up new font-feature-settings
  webgl.dispose();
  const freshWebgl = new WebglAddon();
  term.loadAddon(freshWebgl);
  return { ligatures, webgl: freshWebgl };
}

function disableLigatures(term: Terminal, ligatures: LigaturesAddon, webgl: WebglAddon): WebglAddon {
  ligatures.dispose();
  webgl.dispose();
  const freshWebgl = new WebglAddon();
  term.loadAddon(freshWebgl);
  return freshWebgl;
}
```

This dispose-and-recreate dance for `WebglAddon` is **required** to work around issue `#5455`. Trying to just call `webgl.clearTextureAtlas()` is **not** documented as sufficient — the demo explicitly recreates.

### Customizing the fallback ligature set (useful for macOS/Linux)

Since Local Font Access doesn't work on WKWebView/WebKitGTK, you can fatten the fallback set to match your preferred font. The default set comes from Iosevka calt. To add Fira Code's extra ones:

```ts
new LigaturesAddon({
  fallbackLigatures: [
    // existing defaults
    '<--', '<---', '<<-', '<-', '->', '->>', '-->', '--->',
    '<==', '<===', '<<=', '<=', '=>', '=>>', '==>', '===>', '>=', '>>=',
    '<->', '<-->', '<--->', '<---->', '<=>', '<==>', '<===>', '<====>',
    '::', ':::', '<~~', '</', '</>', '/>', '~~>', '==', '!=', '/=', '~=',
    '<>', '===', '!==', '!===', '<:', ':=', '*=', '*+', '<*', '<*>', '*>',
    '<|', '<|>', '|>', '+*', '=*', '=:', ':>', '/*', '*/', '+++', '<!--', '<!---',
    // add more here e.g. for Fira Code: '|||', '???', '!!!'
  ],
  fontFeatureSettings: '"calt" on, "ss03"',   // optional, default is '"calt" on'
});
```

### Tauri-specific config: nothing needed

- **CSP** (current `default-src 'self'; script-src 'self'; …` in `src-tauri/tauri.conf.json`): no change required. The addon contains no `eval`, no inline scripts, no WebAssembly.
- **Capabilities / Permissions**: `local-fonts` is a browser-level permission handled by WebView2's chrome, not by Tauri's permission system. There is **no Tauri capability to allow-list**.
- **Permissions-Policy**: Tauri does not emit a restrictive Permissions-Policy header by default, so `local-fonts` is permitted by default in the embedded frame.
- **First-run UX caveat**: WebView2 may show a Chromium permission prompt the first time the addon calls `queryLocalFonts()`. Unclear from public evidence whether it auto-approves silently for tauri:// origins — **needs in-vivo verification on Windows**.

### Integration into mini-term's `terminalCache.ts`

The existing `activateWebgl(ptyId)` is the natural place to also wire ligatures. The pattern that respects the load-order note becomes:

```ts
// pseudo, mirrors current activateWebgl + adds ligatures
export function activateWebgl(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry || entry.webglLoaded) return;
  entry.webglLoaded = true;
  try {
    if (useAppStore.getState().config.terminalLigatures) {
      const ligatures = new LigaturesAddon();
      entry.term.loadAddon(ligatures);
      entry.ligaturesAddon = ligatures;  // store for later dispose
    }
    const webgl = new WebglAddon();
    webgl.onContextLoss(() => { /* existing */ });
    entry.term.loadAddon(webgl);
    entry.webglAddon = webgl;
  } catch {
    // WebGL unsupported
  }
}
```

For **runtime toggling** triggered by user changing a setting, replicate the demo's dispose-recreate webgl flow.

---

## 7. Caveats / Not Found

- **No public Tauri-specific report** of either success or failure with `@xterm/addon-ligatures`. Most usage data comes from Electron (VS Code, Hyper, Wave Terminal). The Tauri permission-prompt behavior for `local-fonts` on first call is **unclear** and should be tested locally.
- **No quantitative perf benchmark** of WebGL+ligatures vs WebGL alone. Claims in this report are qualitative based on architecture inspection.
- **CDP/WebView2 command-line flag** `--enable-features=FontAccess` mentioned in old WebView2 issue [#482](https://github.com/MicrosoftEdge/WebView2Feedback/issues/482) is from 2021 and **no longer required** since the feature shipped in stable Chromium 103 (2022-06).
- **Issue `#3303` (WebGL + ligatures rendering remnants)** has been open since 2021 with no triage updates since 2023 — treat as a known cosmetic limitation. Will most commonly manifest under zsh + `zsh-syntax-highlighting` + `zsh-autosuggestions` (autosuggest grey text overlapping ligature shapes). Unlikely to bite mini-term users in typical workflows but worth documenting.
- **`addon-canvas` is not removed from npm**, only from the source tree. Some old guides may still recommend it; do not follow that path.

---

## References (curated)

Primary sources:
- Addon README (note: stale on the Node/Canvas claims): https://github.com/xtermjs/xterm.js/tree/master/addons/addon-ligatures
- Addon typings (authoritative for current behavior): https://github.com/xtermjs/xterm.js/blob/master/addons/addon-ligatures/typings/addon-ligatures.d.ts
- Addon current source (`LigaturesAddon.ts`, `index.ts`, `font.ts`): https://github.com/xtermjs/xterm.js/tree/master/addons/addon-ligatures/src
- Master `package.json` (deps on master, post-cleanup): https://github.com/xtermjs/xterm.js/blob/master/addons/addon-ligatures/package.json
- Demo wiring (definitive integration example): https://github.com/xtermjs/xterm.js/blob/master/demo/client/client.ts
- Published `0.10.0` bundle verified by unpacking https://registry.npmjs.org/@xterm/addon-ligatures/-/addon-ligatures-0.10.0.tgz

Issue tracker:
- `#958` Support font ligatures (historical): https://github.com/xtermjs/xterm.js/issues/958
- `#3303` WebGL + ligatures rendering remnants (open): https://github.com/xtermjs/xterm.js/issues/3303
- `#4000` Selection rendering broken on WebGL+ligatures (open): https://github.com/xtermjs/xterm.js/issues/4000
- `#4362` Infinite loop on huge ligatures in atlas (open): https://github.com/xtermjs/xterm.js/issues/4362
- `#4779` Remove canvas renderer addon (closed, executed): https://github.com/xtermjs/xterm.js/issues/4779
- `#5205` Cursor bg + ligatures on WebGL (closed/fixed): https://github.com/xtermjs/xterm.js/issues/5205
- `#5207` Ligatures don't work in DOM renderer (closed/fixed): https://github.com/xtermjs/xterm.js/issues/5207
- `#5455` font-feature-settings change may not apply (open): https://github.com/xtermjs/xterm.js/issues/5455
- VS Code mirror issue: https://github.com/microsoft/vscode/issues/233005

Browser/API:
- MDN `queryLocalFonts`: https://developer.mozilla.org/en-US/docs/Web/API/Window/queryLocalFonts
- WICG Local Font Access spec: https://wicg.github.io/local-font-access/
- browser-compat-data (Window.queryLocalFonts): https://github.com/mdn/browser-compat-data/blob/main/api/Window.json
- WebView2 / FontAccess legacy flag: https://github.com/MicrosoftEdge/WebView2Feedback/issues/482

Project files reviewed (for integration context):
- `D:\Git\mini-term\package.json` — confirms `@xterm/xterm@^6.0.0` + `@xterm/addon-webgl@^0.19.0`
- `D:\Git\mini-term\src-tauri\tauri.conf.json` — current CSP (no change needed)
- `D:\Git\mini-term\src\utils\terminalCache.ts` — `activateWebgl` is the natural integration point
- `D:\Git\mini-term\src\components\TerminalInstance.tsx` — already mutates `cached.term.options.fontFamily` on font config changes (no addon-side changes needed for fontFamily)

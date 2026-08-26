# Activity Bar tooltip implementation research

## Current behavior

- `crates/mt-app/src/activity_bar.rs` attaches `mt_ui::tooltip::Tooltip` in both `strip_button` and `update_button`.
- `crates/mt-app/src/main.rs` has one additional inline tooltip on `jump-attention`.
- `crates/mt-ui/src/tooltip.rs` documents the current two-stage delay: GPUI's private 500 ms plus the project's 700 ms extra delay.
- The GPUI tooltip anchor follows the pointer, so it cannot provide button-right, vertically centered placement.

## Reviewed behavior

- The first hovered icon in a fresh Activity Bar hover session waits about 500 ms.
- Once a label has appeared, labels for other Activity Bar icons appear immediately.
- Gaps between icons hide the label but preserve the warmed session.
- Leaving the whole Activity Bar resets the session and cancels pending work.

## Recommended ownership

- Keep hover-session state on `Workspace`, because the Activity Bar buttons are assembled in `Workspace::render` and the warm state spans multiple buttons.
- Keep button geometry and the reusable label surface/helper in `activity_bar.rs`.
- Use one source of truth for the initial-delay constant and state transitions.
- Protect delayed callbacks from stale display with task cancellation and/or a session token.

## Paint and event constraints

- Do not use an invisible element that depends on its own hover to become visible; GPUI hidden divs can return from paint before registering their own hover refresh listener.
- Do not defer the entire Activity Bar, because deferred content paints after normal root content and could rise above modal/frost layers.
- The label must paint after the columns so it is not covered, but before drawers, toast, frost, and modal layers.
- Preserve the Activity Bar's 44 px layout reservation while allowing the visual strip/label overlay to paint above the adjacent columns.

## Validation focus

- Pure state-transition tests: delayed first show, immediate warmed switch, gap behavior, full-bar reset, stale timer rejection.
- Compile the `mt-app` target and run focused Activity Bar tests.
- Manual hover check remains useful for exact placement and overlay order.

# Improve top-left menu tooltip behavior

## Goal

Make the icon-only buttons in the left Activity Bar understandable at a glance with VS Code-style grouped hover labels: the first label appears after a short pause, then labels switch immediately while the pointer moves among Activity Bar icons.

## Background

- The Activity Bar buttons are rendered by `crates/mt-app/src/activity_bar.rs` and assembled in `crates/mt-app/src/main.rs:1363`.
- They currently use `mt_ui::tooltip::Tooltip`, whose default path adds 700 ms after GPUI's built-in 500 ms delay, for roughly 1.2 seconds total (`crates/mt-ui/src/tooltip.rs:12-25`).
- GPUI positions that tooltip relative to the mouse pointer, which makes the label appear below and to the right instead of beside the icon.

## Requirements

- Apply the new behavior to the icon-only controls in the left Activity Bar, including conditional controls such as the update and unread-completion buttons.
- When no Activity Bar label has been shown in the current hover session, wait about 500 ms before showing the first label; do not keep the current additional 700 ms delay.
- Once the first label has appeared, moving among Activity Bar icons must switch to the newly hovered icon's label immediately without another delay.
- While the pointer remains inside the Activity Bar, passing through the gaps between icons may hide the current label but must preserve the warmed state.
- Leaving the entire Activity Bar must cancel any pending first-show timer, hide the label, and reset the warmed state so the next entry uses the initial delay again.
- Position the label immediately to the right of the button and vertically center it against the button.
- Preserve the existing tooltip text, theme-compatible visual treatment, button hover/active styling, badges, click actions, and conditional visibility.
- Hide the label when no Activity Bar icon is hovered.

## Out of Scope

- Changing tooltip timing or positioning elsewhere in the application.
- Changing Activity Bar icons, order, dimensions, actions, or translations.
- Adding persistent text labels or expanding the Activity Bar width.

## Acceptance Criteria

- [ ] The first hovered Activity Bar icon shows its label after roughly 500 ms rather than the current roughly 1.2 seconds.
- [ ] After one label has appeared, moving directly or through an inter-icon gap to another Activity Bar icon shows the new label immediately.
- [ ] Leaving and re-entering the Activity Bar restores the initial delay.
- [ ] The label's left edge is outside the button on its right, and the label is vertically centered relative to the 32 px button.
- [ ] Moving away from all Activity Bar icons hides the label and no cancelled timer can later reveal a stale label.
- [ ] Clicking each icon still performs the same action as before; badges and active/hover styles are unchanged.
- [ ] Tooltips outside the Activity Bar retain their existing timing and cursor-relative placement.
- [ ] Relevant formatting, compilation, and focused tests pass.

## Technical Notes

- The existing `.tooltip(...)` hook cannot meet the grouped behavior or fixed positioning: `Tooltip::instant()` still waits for GPUI's private 500 ms delay and remains mouse-relative.
- Keep the hover-session state at the Activity Bar/Workspace level rather than independently inside each button. A generation/token or cancelled `Task` must prevent stale first-show timers.
- Prefer a shared Activity Bar button/label helper over duplicating label markup and hover wiring at each call site.
- Render the Activity Bar label layer above the normal columns but below drawers, toast, frost, and modal layers.
- This is a lightweight, localized UI task; no separate `design.md` or `implement.md` is required.

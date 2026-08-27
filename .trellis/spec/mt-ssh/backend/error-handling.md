# Error Handling

> How errors are handled in this project.

---

## Overview

<!--
Document your project's error handling conventions here.

Questions to answer:
- What error types do you define?
- How are errors propagated?
- How are errors logged?
- How are errors returned to clients?
-->

SSH failures are not a single boolean. A timeout can occur before dispatch,
while channel ownership is uncertain, after the server may have accepted an
exec request, or during cleanup. Callers must preserve that state so they can
choose safely between retry, verification, fallback, and session retirement.

---

## Error Types

<!-- Custom error classes/types -->

- `BoundedExecState` records whether a command was not dispatched, channel open
  is uncertain, enqueue timed out, the reply is uncertain, the request was
  rejected, or execution started.
- `BoundedExecOutput::safe_to_fallback()` is the only affirmative signal for an
  immediate alternate execution path.
- `BoundedExecOutput::requires_session_retirement()` marks sessions whose
  channel state can no longer be trusted.

---

## Error Handling Patterns

<!-- Try-catch patterns, error propagation -->

- Give channel cleanup an independent bounded grace period; an already expired
  command deadline is not a cleanup deadline.
- If a command may have started, verify remote state before fallback. Never
  issue the fallback concurrently with an ambiguous destructive command.
- Evict failed pooled sessions with `evict_if_same(id, expected_arc)`. An old
  task must not remove a newer session that happens to share the connection ID.
- When active leases or extra strong references exist, remove an unhealthy
  session from the cache without forcibly disconnecting other in-flight users.
- On concurrent cache misses, retain and reuse the healthy winner and close the
  losing candidate within a bound.

---

## API Error Responses

<!-- Standard error response format -->

SSH helpers return structured Rust errors/results to their caller. UI-facing
layers translate those errors; this crate must not discard dispatch or cleanup
state merely to produce a shorter message.

---

## Common Mistakes

<!-- Error handling mistakes your team has made -->

- Treating `channel_open_session` cancellation as automatically safe.
- Reusing the command deadline for channel close.
- Evicting by connection ID without checking `Arc` identity.
- Retrying or falling back after a timeout that may have started the command.
- Replacing the original failure with a cleanup failure instead of reporting
  both.

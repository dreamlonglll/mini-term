# Quality Guidelines

> Code quality standards for backend development.

---

## Overview

<!--
Document your project's quality standards here.

Questions to answer:
- What patterns are forbidden?
- What linting rules do you enforce?
- What are your testing requirements?
- What code review standards apply?
-->

The GPUI application starts filesystem and SSH work on background executors.
Correctness therefore depends on preserving operation ownership across entity
updates, project switches, overlays, and late async completions.

---

## Forbidden Patterns

<!-- Patterns that should never be used and why -->

- Inferring a context-menu target from hover or selection after the click.
- Letting a stale async completion mutate the current project or picker state.
- Storing a global file-operation lock only inside the currently rendered file
  tree entity.
- Showing local OS actions such as reveal-in-folder for a remote path.
- Treating UI conflict preflight as proof that the destination is unchanged.

---

## Required Patterns

<!-- Patterns that must always be used -->

- Snapshot local/remote source identity and allocate a request or operation token
  before spawning background work.
- Validate the token and source identity before every UI mutation. Clear shared
  busy state only from the completion that owns it.
- Represent row and blank-area context-menu targets explicitly.
- Revalidate download destinations and transfer conflicts at execution time.
- Keep destructive and transfer operations staged where a partial result would
  otherwise replace a valid destination.

---

## Testing Requirements

<!-- What level of testing is expected -->

- Add pure tests for operation-token ownership, source-identity comparisons,
  blank-area targeting, conflict planning, and stale request rejection.
- Cover project switching and directory refresh during active operations.
- Run Rust compilation, Clippy, and tests in GitHub Actions for this repository;
  local static inspection does not replace the workflow gate.

---

## Code Review Checklist

<!-- What reviewers should check -->

- [ ] Can a late task clear or overwrite state owned by a newer task?
- [ ] Can switching projects bypass an operation lock?
- [ ] Are remote and local menu capabilities separated?
- [ ] Does execution re-check assumptions made by a dialog or preflight?
- [ ] Are cleanup and rollback failures preserved in the reported error?

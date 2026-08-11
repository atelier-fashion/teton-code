---
id: TASK-009
title: "Documentation and two-platform CI confirmation"
status: pending
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-008]
---

## Description

Document the four levels and the status row where a user will meet them, and
confirm the TTY- and width-sensitive tests actually run on both platforms
(AC-13). A green macOS run is not evidence about Linux — TTY detection and
terminal-width handling are platform-specific (LESSON-433).

## Files to Create/Modify

- `docs/` — the user-facing page covering interactive sessions and commands:
  the four levels and what each does, `/permissions` in both forms,
  `default_permission_level` in config, and — stated plainly — that **no level
  affects egress**: `full` grants tool execution and does not touch the privacy
  boundary or the session taint pin
- `.adlc/context/architecture.md` — add the level/table pattern to Key Patterns
  if it generalises beyond this REQ (a named preset over an existing table,
  with the open set classified by `default` rather than enumerated)
- `.github/workflows/ci.yml` — confirm the macOS and Linux legs both run the
  new tests. Change nothing if the existing matrix already covers
  `cargo test --workspace`; record that it does rather than silently assuming it

## Acceptance Criteria

- [ ] **AC-13**: the new unit, piped, and pty tests are confirmed to run on both
      the macOS and Linux CI legs — by reading the workflow and by observing
      both legs green on the PR, not by assertion
- [ ] If `pty_e2e` is skipped on a platform (the existing `daemon_or_skip`
      pattern), that is stated explicitly in the PR body as a known coverage
      boundary rather than counted as a pass
- [ ] Docs name all four levels, both `/permissions` forms, and the config key
- [ ] Docs state the egress orthogonality (BR-3) explicitly — it is the
      guarantee most likely to be assumed away by a reader who sees "full"
- [ ] No `/effort` documentation is added — that is REQ-559's (BR-14)
- [ ] `cargo test --workspace` green; no clippy warnings

## Technical Notes

Check whether `docs/` already has an interactive-session page from REQ-555 /
REQ-556 and extend it rather than adding a parallel one — two pages describing
one surface is the drift BR-15 exists to prevent, in prose.

If the CI matrix turns out **not** to run the pty leg on Linux, say so in the PR
body and treat it as a recorded coverage boundary; do not quietly let AC-13 rest
on the macOS leg alone.

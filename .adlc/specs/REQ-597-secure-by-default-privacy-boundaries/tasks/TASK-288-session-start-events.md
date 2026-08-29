---
id: TASK-288
title: "Publish the unbounded-root warning and the defaults-applied notice at session start"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-285, TASK-286]
---

## Description

BR-5's audit signal, and the System Model's `boundary_defaults_applied` companion. Both are
published from `handle_session_create`, where the root is already derived and the config is
already in hand.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — in `handle_session_create`, after
  `daemon.runtime.session_root_for(...)` and before `ok_string(...)`, publish the two
  session-scoped events.

## Acceptance Criteria

- [ ] BR-5: `unbounded_root_warning` is published exactly once per session create, and only
      when `root.kind` is `Home` or `FilesystemRoot` **and** `effective_boundaries()` is empty.
      Payload carries `root_kind`.
- [ ] The paired negative: same opt-out, same `Home` root, but one unrelated user row declared
      → **no** warning. This pins the condition to the empty set rather than to the opt-out
      flag, and it is the half that would silently pass if the condition were written wrong.
- [ ] `boundary_defaults_applied` is published when the builtin set contributed at least one
      row, with `count` equal to the number of builtin rows composed.
- [ ] Both are published **before** the create response, alongside `PhaseTransition` and
      `rebuild_session_skills`, so a client reading the result cannot receive the session's
      first event after it.
- [ ] A `Plain` or `Project` root with the opt-out set emits no warning.

## Technical Notes

Publish, do not route. These go on the bus session-scoped, like `PhaseTransition` — every
attached client learns, which is BR-5's point. Routing to the creating connection alone would
reproduce exactly the failure REQ-571 BR-4 names: a signal that reaches only the party it
indicts.

The condition's second half is *the effective set*, not `config.boundaries`. After this REQ
the empty effective set is reachable only through `disable_default_boundaries`, so the warning
means "you turned the defaults off, somewhere that matters" — not a startup nag on every home
session. Reading `config.boundaries` here instead would fire the warning on every stock
machine and make the whole REQ look like it had not shipped.

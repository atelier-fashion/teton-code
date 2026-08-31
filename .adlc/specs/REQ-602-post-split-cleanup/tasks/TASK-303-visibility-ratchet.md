---
id: TASK-303
title: "A ratchet on the pub(crate) count, bounded both ways"
status: draft
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-302]
---

## Description

AC-2 and AC-3. The guard is a ratchet, not a search — ADR-2. A search-shaped
test would re-encode the mistake this REQ corrects.

## Files to Create/Modify

- `crates/tetond/tests/runtime_visibility.rs` (new)

## Acceptance Criteria

- [ ] Asserts the `pub(crate)` count under `runtime/` is **exactly 8**, and that
      the five named items are present.
- [ ] Bounded on **both** sides. A drop is as suspicious as a climb: it likelier
      means the selector stopped matching than that the code improved.
- [ ] The pattern is **anchored** so a doc comment discussing `pub(crate)`
      cannot be counted — the miscount that inflated the suppression ratchet
      from 16 to 17, and the one that produced this REQ's own 48–52 estimate.
- [ ] A comment records how to re-derive the number: demote all, build, read
      the errors. A ratchet whose number nobody can reproduce is a number
      nobody will update.
- [ ] **Two mutations, both run and recorded** with what actually went red:
      promote one `pub(super)` item → red; delete one of the five names → red
      the other way.

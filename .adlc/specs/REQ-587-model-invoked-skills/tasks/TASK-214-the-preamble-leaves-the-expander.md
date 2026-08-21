---
id: TASK-214
title: "The frame line leaves `expand`, so two callers can share a body and differ in how it is introduced"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: []
---

## Description

ADR-6, and the one place "one expander, two callers" is not free.

## Files to Create/Modify

- `crates/tetond/src/skills/expand.rs` — `expand` returns the body pieces; the frame line becomes the caller's
- `crates/tetond/src/runtime.rs` — `accept_invocation` supplies the user path's frame verbatim

## Acceptance Criteria

- [ ] The user path's bytes are **unchanged**: `The user invoked /name (a command defined in <display>); the instructions below are that command's body.` Every REQ-585 test that asserts an expansion's bytes stays green without edits — if one needs editing, the split is in the wrong place.
- [ ] `pending_text()` and `fold()` still differ **only** in what fills the slots. `the_measured_text_and_the_folded_text_differ_only_in_the_slots` proves it by reconstruction, not containment; keep it that way.
- [ ] What `skill_fit` measures moves with the frame — the preamble is inside the string `skill_fit` measures today, so scoping it out changes the measured size. Re-state that in the doc so a later reader does not "fix" the arithmetic.
- [ ] `substitute`, the slot scanner, `EXPANSION_CEILING_BYTES` and the envelope neutralization are untouched. ADR-10's rule still holds: `fold` neutralizes **every** string it splices, including the echoed command text.
- [ ] Mutation: leaving the preamble in `expand` fails the byte-equality test AC-2 will write against both callers.

## Technical Notes

- The alternative — `expand` taking a caller enum — was rejected: it puts a caller distinction inside a pure function whose whole value is that it has none, and it makes BR-1's "one expander" claim harder to check, not easier.

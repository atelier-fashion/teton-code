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

- [ ] **The mechanism is `pending_text(frame)` and `fold(frame, outcomes)`** — the frame is a *parameter*, and `assemble` still composes it into the returned string. Prepending it after `expand` returns is the wrong arm: `skill_fit` measures `skill.text` (`runtime.rs:3499`/`:3557`) and `CarriedTurn::begin` seeds the same `String` (`:3604`), so a frame added outside would make Stage A and Stage B **under-measure by the preamble's length** — reopening the band `would_seed_fit`'s 142-byte surcharge exists to close, whose consequence is the middle-elision BR-8 forbids.
- [ ] The user path's rendered bytes are **unchanged**: `The user invoked /name (a command defined in <display>); the instructions below are that command's body.`
- [ ] **All 16 `fold(`/`pending_text(` call sites gain the argument** — 14 in `expand.rs`'s tests, `runtime.rs:3115`, and `provenance_egress.rs:948` (`ran_expansion`). The two `expand.rs` tests that pin the preamble text (`~:660`, `~:708`) are *expected* to change; that is the parameter arriving, not a signal the split is wrong.
- [ ] `pending_text()` and `fold()` still differ **only** in what fills the slots. `the_measured_text_and_the_folded_text_differ_only_in_the_slots` proves it by reconstruction, not containment; keep it that way.
- [ ] **What `skill_fit` measures does not change**, because the frame is still inside the string `assemble` returns. Assert it directly: the input to `skill_fit` is byte-identical to the block `CarriedTurn::begin` seeds.
- [ ] `substitute`, the slot scanner, `EXPANSION_CEILING_BYTES` and the envelope neutralization are untouched. ADR-10's rule still holds: `fold` neutralizes **every** string it splices, including the echoed command text.
- [ ] Mutation: hard-coding the frame inside `assemble` again, and prepending it outside `expand`, each fail a named test — the second by the measured-equals-seeded assertion.

## Technical Notes

- The alternative — `expand` taking a caller enum — was rejected: it puts a caller distinction inside a pure function whose whole value is that it has none, and it makes BR-1's "one expander" claim harder to check, not easier.

---
id: BUG-192
title: "`user_dispatch` is implemented twice — no test can ask both copies the same question"
status: open
severity: low
created: 2026-08-22
updated: 2026-08-22
component: "protocol"
domain: "harness"
stack: ["rust", "daemon", "cli", "json-rpc"]
concerns: ["reliability", "extensibility"]
tags: ["skills", "mirror", "user-dispatch", "shadowing", "model-only", "lesson-528", "req-587-residual"]
---

## Description

The rule deciding whether a skill is user-dispatchable, shadowed, or model-only
exists twice:

- `crates/tetond/src/skills/mod.rs` — `Skill::user_dispatch()`
- `crates/teton/src/slash.rs` — `user_dispatch(&SkillView)`, which additionally
  consults `table_claim` first

Both are unit-tested. Nothing cross-checks them, so the fixed precedence
("shadowing wins over model-only") can drift on one side with both suites green —
LESSON-528's shape.

## Reproduction Steps

1. Change the arm order in one copy only.
2. `cargo test --workspace` — green.

## Expected Behavior

One home for the rule, or a test that reddens when the two disagree.

## Actual Behavior

Two homes, two independent suites, no bridge.

## Root Cause

No in-process bridge is possible: `Skill` is a `tetond` type carrying a `PathBuf`
and `ShadowedBy`, neither of which crosses the wire, and `teton` depends only on
`teton-protocol`/`teton-core` — a `tetond` dependency would invert the
daemon/client boundary the staleness guard exists to keep.

## Resolution

Delete the mirror rather than test around it. The rule reads only
`shadowed.is_some()` and `user_invocable`, and **both are already wire facts on
`SkillView`** — so a pure
`teton_protocol::methods::user_dispatch(shadowed, user_invocable) -> UserDispatch`
can be *the* home. Both sides delegate; the client's `table_claim` check stays a
precondition composed on top, never folded into the shared rule (that distinction
is why the mirror grew a precondition in the first place).

Interim, already landed: an eight-row enumerated case table in the client's tests
naming the daemon function it mirrors.

## Files Changed

- `crates/teton-protocol/src/methods.rs` — the proposed home
- `crates/tetond/src/skills/mod.rs`, `crates/teton/src/slash.rs` — the two copies

---
id: TASK-226
title: "use is the first source of truth"
status: complete
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-225"]
---

## Description

BR-1/AC-1. Every `session/create` and `session/set_cwd` landing on a `project`-kind root records it — hooked beside the skill-registry derivation, inside the same `block_in_place`, so a third call site cannot forget it and the write is not taken on the reader loop (ADR-4, BUG-184's reasoning).

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — the recording call beside `store_session_skills`
- `crates/tetond/src/projects/mod.rs` — `record_root`

## Acceptance Criteria

- AC-1 in full: a create at a `.git` project writes `{name, source: launched, uses: 1}`; a second create bumps `uses` and `last_seen`; `set_cwd` to another project records it too
- creates at `$HOME`, `/`, and a marker-less directory record **nothing** — all three, since they are three different `RootKind`s
- the write is inside `block_in_place` (source-scanned, the shape BUG-184's fix uses)

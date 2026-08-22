---
id: TASK-223
title: "the registry's value types and its ranking"
status: pending
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: []
---

## Description

Leg A's vocabulary, pure and I/O-free (ADR-1). `KnownProject` (path, name, source, first_seen/last_seen, uses), `ProjectSource::{Launched,Scanned}`, `ProjectRegistry` with `record`, `prune`, `rank`, and `MAX_KNOWN_PROJECTS = 128` (ADR-3).

The ranking is ADR-7's total order — match class, then source, then `last_seen`, then `uses`, then **path ascending**. The final tiebreak is load-bearing: without it two otherwise-equal entries rank by hash order and AC-6 becomes platform-flaky (LESSON-540).

## Files to Create/Modify

- `crates/teton-core/src/projects.rs` — new module
- `crates/teton-core/src/lib.rs` — declare it

## Acceptance Criteria

- `MatchClass` orders exact > prefix > substring > path-segment, and a non-match is `None`
- `rank` is a **total** order: an eight-row table with deliberate ties on every key proves the path tiebreak decides, and reversing the input order does not change the output
- `record` bumps `uses` and `last_seen` for a path already present rather than adding a duplicate; a `Scanned` entry becomes `Launched` on first use, never the reverse
- `prune` drops entries a supplied predicate calls dead, and the cap drops the oldest `last_seen`
- no `std::fs` anywhere in the module (asserted by a source scan, the shape `boundary_coverage` uses)

---
id: TASK-225
title: "the store: load, prune, write, and a corrupt file is empty"
status: pending
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-223", "TASK-224"]
---

## Description

`tetond::projects` owns the file (ADR-1/ADR-2). Load-prune-write, with BR-2's pruning at **both** ends and ADR-2's rule that a missing or corrupt file is an empty registry and never an error.

## Files to Create/Modify

- `crates/tetond/src/projects/mod.rs` — new module: `load`, `save`, `record_root`
- `crates/tetond/src/lib.rs` — declare it

## Acceptance Criteria

- a round trip preserves every field
- a missing file loads empty; a **truncated/garbage** file loads empty, logs one line, and does not error — asserted, since this is the fail-open decision
- **AC-2**, the store half: pruning happens at read AND at write — a fixture whose directory is deleted after load is absent from the next save, and one deleted before load is absent from the load
- **AC-2**, the cap half: a registry written over the cap comes back at the cap with the oldest `last_seen` gone
- the file lands in the `DaemonPaths` base (BR-5/AC-5)

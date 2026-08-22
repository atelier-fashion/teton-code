---
id: TASK-229
title: "known names ride the environment line"
status: pending
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-225"]
---

## Description

BR-7, AC-8. For a non-project root the line gains ADR-8's clause, built by adding names while the **rendered whole line** stays within REQ-583's worst-case project row — measured, not arithmetic, so the sweep and the composer cannot drift.

## Files to Create/Modify

- `crates/tetond/src/harness/turn_loop.rs` — the clause, at the existing composer
- `crates/tetond/src/egress/redact.rs` — the resident-ceiling sweeps, if their worst row moves

## Acceptance Criteria

- AC-8 in full: a `home` root carries the clause ordered by `last_seen`; a `project` root carries none; an empty registry carries none; a newline/bidi name renders neutralised
- both resident-ceiling sweeps pass **with constants unchanged** and their worst prompt is still the project row
- ADR-8's three-step shrink is exercised: names fit / only the pointer fits / nothing fits

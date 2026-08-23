---
id: TASK-233
title: "serde tolerance for the two closed payload enums"
status: complete
parent: REQ-588
created: 2026-08-22
updated: 2026-08-22
dependencies: []
---

## Description

BR-4, AC-3. `#[serde(other)] Unknown` on `ContextPressureKind` and `BudgetBound`, both verified still closed at validation. Copies BUG-186's pattern **and its test** rather than inventing a second shape.

Independent of the cap and deliberately first: it is the half with no product questions in it, and it is worth landing even if the cap's design moves (ADR-8).

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — both enums, their `Unknown` arms and doc rationale
- `crates/teton/src/session_ui.rs` — render each `Unknown` as a sane line
- `crates/tetond/src/**` — any exhaustive match the new arms break

## Acceptance Criteria

- **AC-3**: a payload carrying an unknown `ContextPressureKind` parses and renders a line rather than dropping the frame; a payload carrying a known one is **byte-identical** to today
- the same four legs BUG-186 used: unknown degrades, known still parses (non-vacuity), the whole event survives, and `PermissionSubject` is asserted to stay **closed** (its unrecognized arm is a load-bearing refusal)
- mutation-checked: removing either `serde(other)` reddens the test

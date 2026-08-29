---
id: TASK-294
title: "Introduce TurnCore, TurnContext, and DutyContext"
status: complete
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-293]
---

## Description

Define the three context types from ADR-1, with their constructors and test
seams, and **no call-site changes yet**. Landing the types separately from the
migration keeps the type design reviewable on its own and keeps the migration
diffs mechanical.

## Files to Create/Modify

- `crates/tetond/src/turn_context.rs` — new module holding `TurnCore`,
  `TurnContext`, `DutyContext`
- `crates/tetond/src/lib.rs` — declare the module

## Acceptance Criteria

- [ ] `TurnCore<'a>` holds `events: &'a Arc<EventBus>`, `session_id: &'a SessionId`,
      `config: &'a Config`, `router: &'a Router`, and derives `Clone, Copy`.
- [ ] `TurnContext<'a>` holds a `TurnCore<'a>` plus `gate: &'a Arc<PermissionGate>`.
      It does **not** hold `route` (ADR-3).
- [ ] `DutyContext<'a>` holds a `TurnCore<'a>` plus `local_engine` and
      `prompt_spend`, and does **not** hold `gate` (ADR-1).
- [ ] The four core fields are declared **once**, in `TurnCore`, and reached
      through it by both wrappers — not re-declared in each (LESSON-586).
- [ ] `DutyContext` is constructible two ways: from a `TurnContext` (the
      `run_one_attempt` path) and directly from a core (the `spawn_title_session`
      path, which has no gate).
- [ ] BR-6: a unit test constructs each type from test doubles and asserts the
      accessors return them, proving the seams survive.
- [ ] BR-3: none of the three types acquires an id counter, sequence, or
      allocator of any kind. Request-id minting for daemon-wide resources stays
      centralized in `PendingPermissions`; a per-session counter handing out ids
      in a daemon-wide namespace is what cross-authorized tool calls between
      sessions in BUG-161. Verified by reading the struct definitions — these
      types hold borrows and nothing else.
- [ ] BR-4: construction performs **no filesystem I/O and no blocking call**.
      All five fields are already-resolved borrows, so there is nothing for the
      `block_in_place_if_multithread` seam to wrap. A test or doc comment states
      this explicitly, so a later change that adds I/O to a constructor has to
      confront the rule rather than slide past it (BUG-184 — synchronous skill
      discovery on the reader loop stalled RPCs behind a TCC dialog).
- [ ] No `#[allow]` of any kind is added in this task.
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test --workspace
      --no-fail-fast` green with output grepped for `FAILED`.

## Technical Notes

All fields are shared borrows, so both wrappers are `Copy` — pass by value, do
not thread `&TurnContext`.

Carry `gate` as `&Arc<PermissionGate>` rather than `&PermissionGate`:
`permission_gate_for` returns the `Arc`, `build_tools` wants the `Arc`, and the
other consumers auto-deref. The narrower type would force a clone at one site.

Do **not** add convenience methods beyond field access in this task. A context
that starts answering questions stops being a parameter bundle and becomes a
second place for turn logic to live — which is what REQ-599 has to untangle.

---
id: TASK-003
title: "PermissionGate holds a mutable session level; level is evaluated before grants"
status: pending
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-002]
---

## Description

Give `PermissionGate` a session-scoped, mutable level and flip `decide`'s
ordering so the level is consulted **before** session grants (BR-5, ADR-C).
Wire the level-derived denial sentence into the turn loop (BR-2, BR-15).

This is the only behavioural change to the enforcement path in this REQ, and the
one place an existing session could regress — read ADR-C's "consequence worth
stating plainly" before starting.

## Files to Create/Modify

- `crates/tetond/src/harness/permissions.rs`:
  - `PermissionGate`: replace `config: PermissionConfig` with
    `level: Mutex<PermissionLevel>` and `web_allow: Vec<WebTier>`
  - `PermissionGate::new` takes a `PermissionLevel` and the web-allow list
    instead of a built `PermissionConfig` (update the ~12 call sites in
    `crates/tetond/tests/*` and `runtime.rs`/`server.rs`; a test that wants a
    bespoke table keeps one via a `#[cfg(test)]` constructor rather than by
    widening the public API)
  - `fn level(&self) -> PermissionLevel` and
    `fn set_level(&self, PermissionLevel) -> bool` (returns whether it changed)
  - `fn effective_table(&self) -> PermissionConfig` — `table_for(level)` then
    `apply_web_permission(&self.web_allow)`
  - `decide`: consult `effective_table().policy_for(key)` **first**;
    `Allow` → `Allowed` and `Deny` → `Denied` without consulting grants; only
    the `Ask` arm reads a session grant, and only then prompts
  - `fn denial_note(&self, tool: &str) -> Option<String>` — `Some(sentence)`
    when the *level* denies the tool, `None` when the denial came from the user
- `crates/tetond/src/harness/turn_loop.rs` (~line 670): the `Denied` arm uses
  `gate.denial_note(&name)` when present, else today's "the user declined"
  sentence

## Acceptance Criteria

- [ ] **BR-5 / AC-3 (unit)**: allow-always a tool at `guarded`, switch to
      `plan`, the tool is denied; switch back to `guarded` and the grant applies
      again with no re-prompt (assert the prompt count, not just the decision)
- [ ] **AC-2 (unit legs)**: at `guarded` an `edit` asks; at `edits` it allows
      and a `shell` still asks; at `plan` both are `Denied`
- [ ] **BR-7 / AC-15 (unit)**: with a prompt in flight on `shell`, calling
      `set_level(Full)` leaves the waiter registered and unresolved, and the
      user's own answer still decides that call; the inverse leg with
      `set_level(Plan)` likewise does not auto-deny it. Assert against
      `PendingPermissions` state, not by timing
- [ ] **BR-4**: no code path skips `decide` for any level — `full` returns
      `Allowed` from the policy arm. A grep-level assertion is not required
      here (TASK-007 owns the source scans); the test asserts the gate is
      entered and answers
- [ ] The web keys still reach the grant/prompt path at every level (they are
      `Ask` everywhere), so REQ-563's `remembered()` cache-path semantics are
      unchanged — assert a `RejectAlways` on a web key still denies at `full`
- [ ] Full `tetond` suite green, especially `web_consent_matrix`,
      `web_lookup_egress`, `mcp_egress`, `multi_client`
- [ ] `cargo test -p tetond` green; no clippy warnings

## Technical Notes

BR-7 is satisfied **structurally**, not by a guard: `decide` reads the level once
at the top and never again, and nothing on the level-change path touches
`PendingPermissions`. Keep it that way — do not add a level check after the
`rx.await`, which would be exactly the retroactive resolution BR-7 forbids.

Do not hold the level `Mutex` across the `await`. Read it into a local at the top
of `decide` and drop the guard immediately, following the existing "no lock is
held across the await" comment.

`effective_table()` rebuilds a five-entry `HashMap` per decision. That is free
relative to the tool call it gates, and it removes the only place a stale cached
table could survive a level change.

---
id: TASK-270
title: "derive's local branch takes the engine window"
status: draft
parent: REQ-590
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-269]
---

## Description

ADR-2 and ADR-3, and the core of the REQ. `derive`'s `is_local` arm stops returning
`default_pair` and instead supplies `window: LOCAL_ENGINE_N_CTX` (16,384) and
`reservation: LOCAL_GENERATION_RESERVATION` (1,024) into the shared arithmetic, while still
stamping `BudgetBound::LocalEngine`.

Result: **10,240 words / 30,720 bytes**, from `(16,384 − 1,024) = 15,360 usable`, then the
existing `× 2/3` words rule and `× 2` bytes rule.

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — the `is_local` arm of `derive`; `BudgetInputs::local()`
  gains the real window and reservation in place of its two hardcoded zeros

## Acceptance Criteria

- [ ] AC-1: the local pair equals what the remote path yields for a declared window of 16,384
      with the same reservation — one formula, driven from both sides in `derivation_table()`
- [ ] AC-2: `HarnessConfig::default().budget.bound` is still `LocalEngine`, asserted **explicitly
      as the bound**, not via the numbers. Mutation: delete the arm so it falls to the
      `window == 0` path; this must redden. The existing `turn_loop.rs:465-468` pin compares only
      numbers and will stay green — that is why this AC exists
- [ ] AC-3: a test constructs `HarnessConfig::default()` **and** calls `derive` on the local path
      and terminates. Satisfied by the test existing and returning, plus a note at
      `LOCAL_GENERATION_RESERVATION` explaining why it is not `generation_reservation()`
- [ ] AC-4: property over the derivation —
      `budget_bytes / DUTY_REQUEST_BYTES_PER_TOKEN ≤ LOCAL_ENGINE_N_CTX − reservation`.
      **This assertion fails against today's constants**; confirm it does before making it pass
- [ ] AC-5: `max_context = 0` still yields `(4096, 32768)` with `DefaultUnknown`, on a fixture
      that is **not** the local route
- [ ] AC-15: `derive` with a synthetic small window (4,096) does not produce a byte half that,
      at 2 B/token, exceeds that window. Paired with a large window on the same fixture so it
      cannot pass by the floor never applying
- [ ] Full suite: expect breakage in TASK-272's list; do not fix those here

## Technical Notes

`LOCAL_ENGINE_N_CTX` is at `runtime.rs:11999` and is **not** feature-gated — verified: `redact.rs`
has zero `cfg(feature)` gates and already imports it. Do not add a gate or a fallback.

Seams: `derivation_table()` (`budget.rs:2621-2780`) is the table test; `remote(window, cap,
redact_scan)` at `:2607` is the `#[cfg(test)]` builder — construct `BudgetInputs` directly for
the synthetic-window row.

Do **not** change `LOCAL_BUDGET_TOKENS` / `LOCAL_BUDGET_BYTES`. They stay as the no-better-fact
default for `max_context = 0` and the unresolvable route (ADR-4). Two exploration agents assumed
otherwise; the architecture doc records why they were wrong.

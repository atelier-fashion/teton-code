---
id: TASK-236
title: "the check at the choke point"
status: complete
parent: REQ-588
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-234", "TASK-235"]
---

## Description

BR-1/BR-3, AC-1/AC-6. The ceiling is enforced where the spend happens, so no route can bypass it (ADR-2/ADR-3/ADR-4).

A new `EgressError::SpendCeilingReached` raised beside `PrivacyBlocked` — the existing precedent for "the choke point refused this and it is not the provider's fault".

## Files to Create/Modify

- `crates/tetond/src/egress/mod.rs` — the check before the forward point, the new error, the accumulator on `EgressContext`
- `crates/tetond/src/runtime.rs` — one `PromptSpend` per prompt, threaded onto every context that prompt builds

## Acceptance Criteria

- **AC-1**: a prompt whose recorded spend has reached the ceiling is refused at the choke point with the typed outcome, and the ledger shows what was spent
- **BR-3, the load-bearing leg**: the refusal leaves provider health **unchanged** — asserted, because degrading it would make a budget decision look like an outage and reroute later turns away from a healthy provider
- it is **not** a `FailureAction::Fallback`: no cheaper-tier reroute, which is the silent downgrade OQ-4 rejected
- **AC-6**: a provider the price table cannot price, with a ceiling configured, is refused naming the missing price — not waved through uncapped and not charged a guessed rate
- **no ceiling configured ⇒ no check, no pricing lookup, no accumulator**; asserted through a seam, since "off costs nothing" is ADR-6's claim
- mutation check: deleting the check lets a fixture prompt run past its ceiling
- **AC-4**, whose home is here because this is the task that can break it:
  `cargo test --workspace --no-fail-fast` green, and REQ-586's budget and bound
  behaviour **unchanged where no ceiling binds** — the regression that matters
  is the un-opted-in machine, which must be byte-identical to today

---
id: TASK-304
title: "Move the tests that describe moved code, or record why they stay"
status: draft
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Description

BR-7 of REQ-599 was ticked without a check. `views.rs` shipped with **zero**
`#[cfg(test)]` content while its four dedicated `snapshot_from_config` tests
stayed in `mod.rs`, and `engine.rs::local_tier_gated`'s test stayed too.

`engine.rs` and `duty.rs` both document which tests deliberately stayed and why.
`views.rs` says nothing — that silence is the defect, more than the placement.

## Files to Create/Modify

- `crates/tetond/src/runtime/views.rs`, `runtime/engine.rs`, `runtime/mod.rs`

## Acceptance Criteria

- [ ] The four `snapshot_from_config` tests and `local_tier_gated`'s test either
      move to their subject's module, or each module header names them and says
      why they stayed — the distinction being whether the test's *subject* moved
      or it merely uses the moved item as a fixture.
- [ ] `views.rs`'s header addresses tests either way. Its current silence is
      what makes the placement unreviewable.
- [ ] No test is left asserting against a module it no longer describes.
- [ ] Suite green, grepped for `FAILED`.

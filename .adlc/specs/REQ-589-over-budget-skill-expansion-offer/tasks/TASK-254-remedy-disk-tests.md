---
id: TASK-254
title: "Verify the remedy write on disk, and the attestation posture"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-250]
---

## Description

AC-13 + AC-20 / LESSON-519 + LESSON-520. Inspect the artifact, do not infer from a return code — and pair every refusal with an accepted counterpart on the same fixture or the refusal test is vacuous.

## Files to Create/Modify

- `crates/tetond/tests/config_set_attestation.rs` — AC-20 on both presence configurations
- `crates/tetond/tests/config_preservation.rs` — the on-disk double check

## Acceptance Criteria

- [ ] The applied remedy is verified by reading the config FILE and re-parsing it — both, per `a_field_less_registration_preserves_the_stored_window_and_a_declared_one_writes_it` (config_preservation.rs:885)
- [ ] The refusal leg asserts the config is byte-identical before and after, on the same fixture (LESSON-520)
- [ ] AC-20 runs on a build with and without `presence`, using `TETON_PRESENCE_ACCEPT=1` and `=fail`
- [ ] The ordering invariant is tested by failing the SECOND write and asserting the config never reaches the forbidden state (ADR-5)
- [ ] `verified_on` is recorded alongside the written window (ADR-7)

## Technical Notes

`config_set_attestation.rs:37` is the narrowest existing fixture for the presence seam and the most directly reusable.

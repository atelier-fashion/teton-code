---
id: TASK-299
title: "Suppression count ratchet and the preservation checks"
status: draft
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-295, TASK-296]
---

## Description

AC-1's regression ratchet, plus explicit verification of the three preservation
criteria (AC-6, AC-7, AC-9) that this REQ must not break.

## Files to Create/Modify

- `crates/tetond/tests/suppression_ratchet.rs` — the AC-1 count test

## Acceptance Criteria

- [ ] The ratchet walks the **source tree**, not clippy output (ADR-6). Clippy
      cannot see `engine.rs`'s two sites: they are behind
      `#[cfg(feature = "llama")]`, which neither CI nor AC-3's command compiles.
      LESSON-515 is this exact failure mode.
- [ ] The ratchet asserts the workspace count is `<=` the number the refactor
      actually reached, and that number is written into the test as a constant
      with a comment naming this REQ.
- [ ] The test additionally asserts the count is **not below** the expected
      number without an update — a silent drop means a suppression was deleted
      while its lint was disabled, not that the code improved.
- [ ] AC-6: the `ParkingVerifier` reader-loop test in
      `crates/tetond/tests/multi_client.rs` still proves concurrent RPCs are
      served while a presence gate blocks. Run it and confirm; state plainly in
      the PR body that it is a **preservation** check — this REQ introduces no
      new filesystem I/O on the construction path, so the guard cannot fail for
      a reason this REQ created.
- [ ] AC-7: the gate-before-parse refusal tests still pair a valid, persistable
      payload with an acceptance case (LESSON-520). Confirm by reading the
      tests, not by observing that they pass.
- [ ] AC-9: each of `PrivacyBlocked`, `ContextLengthExceeded`, and
      `SpendCeilingReached` still has **both halves** — `failure_class() -> None`
      and a dedicated arm ordered before the generic remote arm. A test inverts
      each arm and confirms the user-facing sentence changes.
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test --workspace
      --no-fail-fast` green with output grepped for `FAILED`.

## Technical Notes

AC-1 requires the PR body to report the drop **split into two populations**:
vestigial (suppressed nothing) and earned (the function genuinely stopped
tripping the lint). Phase 1 measured 9 vestigial of 25. Report what actually
landed, not the estimate.

For AC-9's inversion: removing a dedicated arm should make the outcome fall
through to the generic remote arm, whose sentence is "provider failed
unrecoverably" — wrong about the cause and naming no remedy (conventions.md,
LESSON-557). If inverting an arm does **not** change the sentence, that is a
finding to report, not a test to soften.

The measuring command that makes the whole workspace report — needed because
`clippy::all = deny` aborts at the first erroring crate:

    cargo clippy --workspace --all-targets -- -A clippy::all -W clippy::too_many_arguments

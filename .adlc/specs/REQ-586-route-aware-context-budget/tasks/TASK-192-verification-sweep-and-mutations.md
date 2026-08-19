---
id: TASK-192
title: "Verification sweep: workspace --no-fail-fast, margin tests, mutation checks on the bound/refit/report, constants one-home grep, guide headroom untouched"
status: draft
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-191"]
repo: teton-code
---

## Description

The AC-13 gate and the adversarial pass before `/proceed` Phase 5: run the
whole workspace, prove the pins bite, and prove no number grew a second home.

## Files to Create/Modify

- (no new source) `crates/tetond/src/harness/budget.rs`, `router.rs`, `context.rs`, `egress/redact.rs` — only if a mutation below survives: add the missing assertion.
- This task file — a "Verification log" section with the failing test per mutation.

## Acceptance Criteria

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast` green (grep the log for `FAILED` — LESSON-533).
- [ ] Mutations each make ≥ 1 test fail (apply, run, revert; record the failing test): (a) `derive()` returns `Window` for `is_local`; (b) Degrade arm builds from `CapabilityProfile::default()` again; (c) reroute arm skips `rebudget`; (d) `truncate_to_budget` returns a zeroed report; (e) `REMOTE_TOKENS_PER_WORD` 3/2 → 1/1; (f) `RegisterProvider` replaces instead of merges; (g) `ContextLengthExceeded` given a `FailureClass`; (h) `REDACT_SCANNABLE_CONTEXT_BYTES` replaced by the literal `89_127` — the assertion passes (expected) but the one-home grep below catches it.
- [ ] `grep -rn "89_127\|89127" crates/ --include=*.rs | grep -v "//"` is empty; `4_096`/`32_768`/`1_500`/`12_000` each have exactly one non-test home.
- [ ] `git diff main -- crates/tetond/src/harness/self_config.md` is empty; both prompt-margin tests green with the ceiling unchanged.
- [ ] Every task file in this REQ has status `complete`; the spec's automated AC checkboxes are ticked with the test name.

## Technical Notes

- LESSON-441 (a fix pass is new code — re-verify adversarially).
- Commit as `chore(REQ-586): verification sweep — mutations, one-home grep, headroom [TASK-192]`.

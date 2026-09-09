---
id: TASK-407
title: "End to end: a cleared command does not pin, the next prompt's bytes reach the provider, and a boundary read with a redirect still pins for good"
status: draft
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: ["TASK-403", "TASK-404", "TASK-405"]
repo: teton-code
---

## Description

Prove BR-9 and the charter-level outcome the way LESSON-550 and LESSON-650 require: on a
route bound to a mock remote provider, the model's first shell call is
`ls src 2>/dev/null && echo ok 2>&1`; assert no `session_pinned`, that the next prompt's
`route_decided` names the provider, and that the provider received that prompt's bytes.
Prove the inverse for `cat secrets/prod.env 2>/dev/null`. Check `teton doctor` and the
routing notice (AC-10).

## Files to Create/Modify

- `crates/tetond/tests/provenance_egress.rs` — `a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider` using `CaptureSse::with_bodies`, `sse_turn(.., Some((id, "shell", args)))`, `captured()`; and `a_redirect_does_not_hide_a_boundary_read` asserting `privacy_block`, `session_pinned { cause: boundary_hit, liftable: false }`, zero provider calls after
- `crates/tetond/tests/e2e/shell_pin_shape.rs` — `/shell allow` after the boundary case does not lift (BR-8)
- `crates/tetond/tests/e2e/model_identity.rs` or the doctor e2e — after the cleared command, `teton doctor`'s session line and `route_decided.reason` name the provider and no pin

## Acceptance Criteria

- [ ] No `session_pinned` event across both turns; second `route_decided.provider_id` is the mock's; `captured().len() == 2` and the second body contains the second prompt's text
- [ ] The boundary case publishes `privacy_block { path: secrets/prod.env }` and `session_pinned { cause: boundary_hit }`; `/shell allow` leaves it pinned
- [ ] `route_decided.reason` for the cleared turn does not contain "pinned"
- [ ] The tests fail red when TASK-403's strip is reverted (run the inversion and record it in the mutation record)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/tests/provenance_egress.rs::a_redirect_does_not_hide_a_boundary_read` | yes |
| BR-8 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::shell_allow_does_not_lift_a_boundary_hit_behind_a_redirect` | no |
| BR-9 | test-case | `crates/tetond/tests/provenance_egress.rs::a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider` | yes |
| AC-3 | test-case | `crates/tetond/tests/provenance_egress.rs::a_redirect_does_not_hide_a_boundary_read` | yes |
| AC-4 | test-case | `crates/tetond/tests/provenance_egress.rs::a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider` | yes |
| AC-10 | test-case | `crates/tetond/tests/e2e/model_identity.rs::a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider` | no |

## Technical Notes

- `temp_repo()` in `provenance_egress.rs` already plants `secrets/` under a `secrets/**`
  boundary; reuse it. Plant the leak marker only in `secrets/prod.env`'s bytes
  (LESSON-624), never in the command.
- Assert bytes at the mock, not `route_decided` text alone (LESSON-650).
- The `sh` on the runner matters for exit codes, not verdicts (BR-10) — do not assert on
  the tool result's exit status.

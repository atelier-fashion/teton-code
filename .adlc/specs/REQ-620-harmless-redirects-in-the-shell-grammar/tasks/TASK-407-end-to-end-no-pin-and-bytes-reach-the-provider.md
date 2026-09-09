---
id: TASK-407
title: "End to end: a cleared command does not pin, the next prompt's bytes reach the provider, and a boundary read with a redirect still pins for good"
status: complete
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

- `crates/tetond/tests/provenance_egress.rs` — `a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider` and `a_redirect_does_not_hide_a_boundary_read`, both over a `CarriedTurn` so the commit seam's pin is in scope; `drive_scripted_turn` extracted to `drive_bodies` so a second **prompt** turn (no tool call) drives the same loop, choke point and boundaries
- `crates/tetond/tests/e2e/shell_pin_shape.rs` — `shell_allow_does_not_lift_a_boundary_hit_behind_a_redirect` (BR-8) and `a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider` (AC-10)
- `crates/tetond/src/harness/tools/shell_syntax.rs` — the mutation record gains this task's integration reds (conventions.md: the count is owned by the last task to add tests to the rule)

**AC-10 did not land in `model_identity.rs`.** That file has a remote-route
fixture but no doctor fixture, and two facts decide against adding one there:
`teton doctor`'s report is daemon-scoped (header, attach line, `config/get`
providers, postures, trailer) and has no session or pin line at all, and the
`teton` CLI binary belongs to another package, so `CARGO_BIN_EXE_teton` is not
defined in `tetond`'s test binaries. The daemon half of AC-10 is asserted in
`shell_pin_shape.rs` on what a client received — `route_decided`, the
`shell/override` pin RPC, and the `config/get` snapshot doctor renders — and
the test's doc comment says so. A spawned `teton doctor` belongs to
`crates/teton`'s own CLI end-to-end suite.

## Acceptance Criteria

- [x] No pin across both turns; the second `route_decided.provider_id` is the mock's; the second prompt's own bytes are on the wire. **Counts, and why they are what they are:** `provenance_egress.rs` drives one capture transport *per turn*, so turn 1 captures 2 (the tool call, and the send carrying its result) and turn 2 captures 1 (the second prompt) — `captured().len() == 2` then `== 1`, asserted separately, with `contains_bytes(&second[0], SECOND_PROMPT)` for the bytes. The e2e counts the same three at one shared mock: `request_count() == 2` after prompt 1, `== 3` after prompt 2. The `session_pinned` **event** is asserted absent in the e2e, where the daemon publishes it; in `provenance_egress.rs` the publisher (`TaintingPrivacySink`) is `pub(super)` and unreachable, so the absence is asserted on the three values the event is built from — `SessionTaint::cause`, `liftable`, `SessionTaint::reason` — and the test says so rather than asserting a vacuous absence.
- [x] The boundary case publishes `privacy_block { path: secrets/prod.env }` and `session_pinned { cause: boundary_hit, liftable: false }` with no `reason`; `/shell allow` is refused and the session stays local (one route to the provider, one request, nothing later leaves)
- [x] `route_decided.reason` for the cleared turn does not contain "pin" (the pinned route's own sentence is "…is pinned to the local tier…")
- [x] The tests fail red when TASK-403's strip is reverted. **Inversion run 2026-09-09, restored:** `strip_null_redirects` made a no-op — all four new tests red, and the six REQ-614/BUG-214/BUG-215/REQ-619 tests in `shell_pin_shape.rs` plus 20 of `provenance_egress.rs`'s 22 stay green. Recorded in each test's doc comment and in `shell_syntax.rs`'s mutation record.

**AC-1 is discharged at the unit level and is not re-driven e2e.** The literal
2026-09-09 command is a *grammar* claim, and TASK-403's
`shell_provenance::the_2026_09_09_command_is_rooted_without_its_home_probe`
asserts both halves of AC-1 — `rooted` without the `~/bin` probe, `unknown`
naming an out-of-root path with it — against fixture roots the classifier's own
module mints. Driving it through the e2e shell path would need a fixture repo
carrying `.adlc/`, `tools/lint-skills/` and a `$HOME` with `bin/adlc-read`, and
would still be asserting the classifier's answer through six layers rather than
the consequence this task owns. The e2e commands here are the two-form
reduction of it (`ls src 2>/dev/null && echo ok 2>&1`), which reaches the
consequence with nothing incidental in the way.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/tests/provenance_egress.rs::a_redirect_does_not_hide_a_boundary_read` | yes |
| BR-8 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::shell_allow_does_not_lift_a_boundary_hit_behind_a_redirect` | no |
| BR-9 | test-case | `crates/tetond/tests/provenance_egress.rs::a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider` | yes |
| AC-4 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider` | yes |
| AC-3 | test-case | `crates/tetond/tests/provenance_egress.rs::a_redirect_does_not_hide_a_boundary_read` | yes |
| AC-4 | test-case | `crates/tetond/tests/provenance_egress.rs::a_null_redirect_does_not_pin_and_the_next_prompt_reaches_the_provider` | yes |
| AC-10 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::a_cleared_shell_call_leaves_doctor_and_the_route_on_the_provider` | no |

## Technical Notes

- `temp_repo()` in `provenance_egress.rs` already plants `secrets/` under a `secrets/**`
  boundary; reuse it. Plant the leak marker only in `secrets/prod.env`'s bytes
  (LESSON-624), never in the command.
- Assert bytes at the mock, not `route_decided` text alone (LESSON-650).
- The `sh` on the runner matters for exit codes, not verdicts (BR-10) — do not assert on
  the tool result's exit status.

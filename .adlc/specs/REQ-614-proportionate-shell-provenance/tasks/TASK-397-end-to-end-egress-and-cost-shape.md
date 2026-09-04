---
id: TASK-397
title: "End-to-end — egress capture for the three verdicts and the cost.db shape that cannot recur"
status: draft
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-391, TASK-392, TASK-395, TASK-396]
---

## Description

The acceptance suite that makes the REQ's central claim checkable: the
2026-09-04 session's shape — one remote call, then sixty-five local ones —
cannot happen again for a session that only ran `pwd`, `ls -la`,
`git status` and `git log -3`.

Egress capture, not code inspection: conventions.md requires a mock
transport asserting no boundary content in any remote payload for any BR-1
claim.

## Files to Create/Modify

- `crates/tetond/tests/provenance_egress.rs` — AC-1, AC-2, AC-3, AC-8 egress-capture cases
- `crates/tetond/tests/e2e/shell_pin_shape.rs` — AC-12's scripted six-prompt session over the cost ledger
- `crates/tetond/tests/egress_capture.rs` — extend the existing capture harness if a seam is missing

## Acceptance Criteria

- [ ] AC-1: builtin boundaries in force, `build` routed to a remote provider; `shell: ls -la` then a second prompt — the second prompt's request **body leaves**, and the session carries no taint
- [ ] AC-2: `shell: cat .env` in the same session pins with `boundary_hit`; `/shell allow` is refused naming the cause; every later `route_decided.reason` names the pin; **no later request leaves the machine**
- [ ] AC-3: `shell: curl https://example.com` pins with `unknown_shell`; the standing pin line prints once; `/shell allow` lifts, writes one row, and the next prompt routes remotely; a second `/shell allow` writes no row
- [ ] AC-11: the transcript for a pinned session contains one `session_pinned` record **before** the first pinned `route_decided`, and one `session_pin_lifted` after `/shell allow` — ordering asserted, not just presence
- [ ] AC-12: a scripted session running `pwd`, `ls -la`, `git status`, `git log -3` across four prompts records **every** agent-turn row against the remote provider. A fifth prompt running `cargo test` pins with `unknown_shell`. `/shell allow` and a sixth prompt record that prompt's rows against the remote provider again
- [ ] The egress-leak marker used by any capture assertion lives **only in the guarded file's bytes** — never in a tool argument, grep pattern, prompt or file name (LESSON-624, conventions.md)

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/tests/provenance_egress.rs::an_unknown_shell_result_still_blocks_its_own_turn` | yes |
| AC-1 | test-case | `crates/tetond/tests/provenance_egress.rs::ls_la_then_a_second_prompt_reaches_the_remote_provider` | yes |
| AC-2 | test-case | `crates/tetond/tests/provenance_egress.rs::cat_dotenv_pins_permanently_and_nothing_later_leaves` | no |
| AC-3 | test-case | `crates/tetond/tests/provenance_egress.rs::curl_pins_liftably_and_shell_allow_restores_routing` | yes |
| AC-11 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::pinned_session_records_the_two_events_in_order` | no |
| AC-12 | test-case | `crates/tetond/tests/e2e/shell_pin_shape.rs::the_2026_09_04_cost_shape_cannot_recur` | yes |

## Technical Notes

- AC-12 is the REQ's headline claim and the one the description says was
  *inferred* from `cost.db` shape rather than observed. Write it so it fails
  loudly on the old behavior: run it against the pre-REQ classifier (or stub
  the verdict back to a constant `Unknown`) and confirm it goes red, then
  record the observation.
- AC-2's "no later request leaves the machine" is an assertion of **absence**
  — count the captured requests, do not merely check that the ones present
  look local (LESSON-550).
- Rebuild binaries before any end-to-end run: a stale binary passing an e2e
  assertion is a false green this repo has paid for before.

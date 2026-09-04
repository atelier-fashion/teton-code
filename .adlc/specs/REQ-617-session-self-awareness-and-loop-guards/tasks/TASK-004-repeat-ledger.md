---
id: TASK-004
title: "The per-turn repeat ledger: an identical call is refused by the harness before dispatch"
status: draft
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

BR-4, BR-5 and BR-6. The rule the harness enforces for `skill` and no other tool
becomes a rule for every tool, at the one point in the loop where a dispatch is
about to happen (ADR-4).

## Files to Create/Modify

- `crates/tetond/src/harness/repeat.rs` — new. `CallFingerprint`,
  `RepeatLedger`, `RepeatVerdict`, the read-only verb table, and the refusal
  sentence.
- `crates/tetond/src/harness/mod.rs` — declare the module.
- `crates/tetond/src/harness/turn_loop.rs` — the gate in `run_the_allowed_tool`
  immediately before `tools.dispatch`; `TurnLatches` gains the ledger and its
  doc comment is rewritten to match what it now holds.
- `crates/tetond/tests/repeat_refusal.rs` — new. AC-4, AC-5, AC-6.

## Acceptance Criteria

- [ ] AC-4: five identical `shell: ls -la` in one turn against a stub model
      dispatch **once** and refuse **four** times; each refusal carries BR-4's
      sentence with the byte count of the first result; `tool_call_repeated` is
      emitted four times; no duty call is recorded for the refusals.
- [ ] AC-5: two identical `edit` calls dispatch; the third refuses. Two identical
      `read` calls: the second refuses. `ls -la` then `ls -la .`: both dispatch.
- [ ] AC-6: a new prompt turn dispatches the same call again.
- [ ] The refusal rides **outside** the untrusted frame, in the same slot as the
      over-budget refusal — asserted, not asserted-by-inspection.
- [ ] `tool_call_repeated`'s payload carries `tool`, `count` and `turn_id` and
      **no arguments** — asserted by a test that puts a distinctive string in the
      arguments and greps the published event for it.
- [ ] Benign path: a turn of four *different* calls refuses nothing and emits no
      `tool_call_repeated`.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/tetond/tests/repeat_refusal.rs::five_identical_shell_calls_dispatch_once_and_refuse_four_times` | yes |
| BR-5 | test-case | `crates/tetond/tests/repeat_refusal.rs::a_repeat_refusal_rides_outside_the_untrusted_frame` | no |
| BR-6 | test-case | `crates/tetond/tests/repeat_refusal.rs::identical_means_identical_and_a_new_turn_starts_empty` | yes |
| AC-4 | test-case | `crates/tetond/tests/repeat_refusal.rs::five_identical_shell_calls_dispatch_once_and_refuse_four_times` | yes |
| AC-5 | test-case | `crates/tetond/tests/repeat_refusal.rs::write_capable_tools_get_a_second_chance` | yes |
| AC-6 | test-case | `crates/tetond/tests/repeat_refusal.rs::identical_means_identical_and_a_new_turn_starts_empty` | yes |

## Technical Notes

`skill` is **excluded** from the ledger — it keeps REQ-587's own counter, which
already implements BR-6b's "nothing completed in between" admission that this
simpler rule does not have. Adding `skill` here would refuse calls REQ-587
deliberately admits.

Read-only verb set, shared with REQ-615's write set once either lands:
`ls pwd cat head tail find grep wc echo` plus `git status` and `git log`.
Unknown verbs are **write-capable** — the fail-safe direction, because
mis-classifying a write as read-only refuses a legitimate retry after a real
change.

---
id: TASK-430
title: "e2e: agent_dispatch.rs — concurrency, claim, consent, tiers, boundary, spend shares, the status matrix, skills, transcript"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-07
dependencies: ["TASK-428", "TASK-423", "TASK-429"]
repo: teton-code
---

## Description

The acceptance suite against a spawned daemon and the matching/rendezvous `MockProvider`.
Covers AC-5 (rendezvous proves concurrency), AC-6 (parked verifier: a child's `tool_started`
reaches a subscriber while the child is parked, and a second prompt is refused busy), AC-7/8
(consent at `guarded`, shared grant, unattended deny, `plan` read-only), AC-9 (tier routing),
AC-10 (egress capture: a local-only read pins child and parent), AC-13 (two-child split and
three-child release derived from the ledger), AC-14 (eight statuses), AC-16 (skill in a child;
user-skill pins the parent), AC-17 (one transcript file). Every gate test runs its inversion
and records the red count (conventions "run the inversion on every test in the batch").

## Files to Create/Modify

- `crates/tetond/tests/agent_dispatch.rs` — new: the suite, one `mod` per AC group
- `crates/tetond/tests/e2e/harness.rs` — any helper the suite needs beyond TASK-423 (a `spawn_with_agent_config` builder)
- `crates/tetond/tests/provenance_egress.rs` — the AC-10 capture assertion beside the existing boundary tests
- `crates/tetond/tests/event_response_ordering.rs` — child events excluded from the parent golden sequence; per-child order asserted separately (LESSON-591)

## Acceptance Criteria

- [x] AC-5: `rendezvous(3)` releases; three `agent_child_started` precede every `agent_child_finished`
- [x] AC-6: subscriber receives a child's `tool_started` while the child's tool is parked; a second `prompt` is refused busy during the call
- [x] AC-7/AC-8 consent matrix as written in the spec
- [x] AC-10: `assert_no_boundary_bytes` over every request after the child's local-only read, from both child and parent; and the benign twin — a child that touches no boundary leaves the parent's next call remote
- [x] AC-13: shares and release derived from the ledger's recorded spend, never a literal
- [x] AC-14: one test per status, parent continues in each
- [x] AC-16, AC-17 as written
- [x] Inversion counts recorded in each test's doc comment; a batch with zero reds is a finding, not a pass

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-5 | test-case | `crates/tetond/tests/agent_dispatch.rs::concurrency::three_children_rendezvous` | no |
| AC-6 | test-case | `crates/tetond/tests/agent_dispatch.rs::liveness::child_tool_started_arrives_while_parked` | no |
| BR-4 | test-case | `crates/tetond/tests/agent_dispatch.rs::liveness::second_prompt_refused_busy_during_call` | yes |
| AC-7 | test-case | `crates/tetond/tests/agent_dispatch.rs::consent::guarded_child_asks_and_sibling_reuses_grant` | yes |
| AC-8 | test-case | `crates/tetond/tests/agent_dispatch.rs::consent::plan_child_edit_denies` | no |
| BR-5 | test-case | `crates/tetond/tests/agent_dispatch.rs::consent::unattended_unlisted_gate_denies_typed` | yes |
| AC-9 | test-case | `crates/tetond/tests/agent_dispatch.rs::routing::build_child_under_think_parent` | yes |
| AC-10 | test-case | `crates/tetond/tests/provenance_egress.rs::child_local_only_read_pins_child_and_parent` | no |
| BR-9 | test-case | `crates/tetond/tests/provenance_egress.rs::child_local_only_read_pins_child_and_parent` | no |
| BR-9 | test-case | `crates/tetond/tests/provenance_egress.rs::child_without_boundary_touch_does_not_pin_parent` | yes |
| AC-13 | test-case | `crates/tetond/tests/agent_dispatch.rs::spend::two_child_split_and_three_child_release` | yes |
| BR-8 | test-case | `crates/tetond/tests/agent_dispatch.rs::spend::two_child_split_and_three_child_release` | yes |
| AC-14 | test-case | `crates/tetond/tests/agent_dispatch.rs::statuses::*` (eight cases) | yes |
| BR-10 | test-case | `crates/tetond/tests/agent_dispatch.rs::statuses::*` (eight cases) | yes |
| AC-16 | test-case | `crates/tetond/tests/agent_dispatch.rs::skills::user_skill_in_child_pins_parent` | no |
| BR-12 | test-case | `crates/tetond/tests/agent_dispatch.rs::skills::user_skill_in_child_pins_parent` | no |
| AC-17 | test-case | `crates/tetond/tests/agent_dispatch.rs::transcript::one_file_parent_and_children` | no |
| BR-13 | test-case | `crates/tetond/tests/event_response_ordering.rs::child_events_excluded_from_parent_golden` | yes |

## Technical Notes

- LESSON-624: egress-leak markers live only in the guarded file's bytes — never in a task string, which is echoed to the provider legitimately.
- LESSON-518: the parked verifier must run on a multi-thread runtime and provably park inside the child's tool.
- LESSON-533: run `cargo test --workspace --no-fail-fast` and grep for `FAILED`.
- Conventions "assert the frame the product owns": a child's `shell` output assertions parse `(exit N)`, not platform text.
- Child names in fixtures partition any files children write (LESSON-610/611).

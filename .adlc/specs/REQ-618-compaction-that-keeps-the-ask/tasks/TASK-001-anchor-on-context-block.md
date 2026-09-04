---
id: TASK-001
title: "Anchor on ContextBlock, assigned by one walk and re-stated at every seeding seam"
status: complete
parent: REQ-618
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

Introduce `Anchor` and make it a required field on `ContextBlock`, then give the
manager the one function that assigns it. Nothing consumes the anchor yet — that
is TASK-002 and TASK-003 — so this task lands the vocabulary, the 36 call-site
statements, the re-statement seams, and the guard that keeps the assignment
harness-only.

## Files to Create/Modify

- `crates/tetond/src/harness/context.rs` — `Anchor` enum (`None` / `UserAsk` / `SkillBody`), `ContextBlock.anchor`, `ContextManager::restate_anchors`, calls from `push_user` / `push_user_from` / `replay`
- `crates/tetond/src/carry.rs` — `restate_anchors` after the replay + seed in `CarriedTurn::begin`; `anchor:` on 7 literals
- `crates/tetond/src/sessions.rs`, `src/egress/provenance.rs`, `src/repo_context/render.rs`, `src/harness/tools/mcp.rs`, `src/harness/turn_loop.rs`, `src/harness/compact.rs`, `src/runtime/duty.rs` — `anchor: Anchor::None` on existing literals
- `crates/tetond/tests/{provenance_egress,redact_egress,prefix_cache_session}.rs` — same
- `crates/tetond/tests/suppression_ratchet.rs` — the region check below

## Acceptance Criteria

- [x] `Anchor` has exactly three variants and no `Default`; every `ContextBlock`
      literal in the workspace states one, so a new push path that forgets is a
      compile error.
- [x] `restate_anchors` assigns `UserAsk` to the newest two **prompt** blocks
      (`BlockRole::User` + `Provenance::User{..}`), keeps `SkillBody` only on a
      block newer than the newest prompt block, and `None` to everything else.
- [x] `CarriedTurn::begin` re-states after `replay` and `push_user_from`, so a
      block anchored two prompts ago is `None` on the third (BR-8).
- [x] A tool result whose text contains `anchor: user_ask` is pushed with
      `Anchor::None` (AC-6) — asserted on the pushed block, not on a sanitizer.
- [x] A source-scanning region check fails if any `anchor:` initializer outside
      `harness/context.rs` names anything but `Anchor::None`, and the check's own
      inversion is recorded in its doc comment.
- [x] `cargo test --workspace --no-fail-fast` green; no `FAILED` in output.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `harness::context::tests::the_newest_two_prompt_blocks_are_the_ask` | yes |
| BR-2 | test-case | `harness::context::tests::a_skill_body_anchor_lapses_on_the_next_prompt` | yes |
| BR-3 | structural-check | `tests/suppression_ratchet.rs`: `only_the_context_manager_assigns_an_anchor` | yes |
| BR-8 | test-case | `tests/conversation_carry.rs::anchors_are_restated_across_the_carry` | yes |
| AC-6 | test-case | `harness::context::tests::a_tool_result_naming_an_anchor_is_not_one` | yes |

## Technical Notes

`restate_anchors` reads `role`, `provenance` and position only. Block *text* is
never an input — that is what makes BR-3 structural rather than a sanitizer.
Follow ADR-618-1; the `system_sources` re-statement in `CarriedTurn::begin` is
the precedent to copy, including its comment about ordering after the replay.

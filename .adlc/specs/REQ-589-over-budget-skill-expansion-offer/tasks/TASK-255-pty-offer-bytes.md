---
id: TASK-255
title: "PTY leg pinning the offer's rendered bytes"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-251]
---

## Description

AC-14 / BUG-191. Assert what the terminal actually prints, not what a structure says it would print.

## Files to Create/Modify

- `crates/teton/tests/pty_e2e.rs`

## Acceptance Criteria

- [x] The offer's question, figures, and remedy line are asserted against the transcript verbatim
- [x] Answering each of the four options completes the turn as expected
- [x] Uses `wait_for` polling with the existing deadline — never a fixed sleep (LESSON-450)

## Technical Notes

`the_acknowledgment_prompt_names_the_root_its_skills_and_what_it_left_out` (pty_e2e.rs:1521) is the pattern: script the local engine via TETON_LOCAL_SCRIPT, wait for the rendered marker, assert `.contains` on the transcript.

## Implementation Notes

Two legs, both against the reported failure's own cell of the reachability
table — a typed `/analyze` from a repository's `.claude/skills`, Stage A,
`bound: local engine`, verdict `WindowUnknown`, remedy `BindTierRemote`:

- `the_over_budget_offer_is_drawn_at_a_terminal_in_the_daemons_own_words`
- `each_over_budget_answer_settles_the_turn_the_way_its_label_said`

**ADR-16 holds at the terminal.** The daemon's composed sentence reaches the
screen byte for byte — the whole sentence is asserted as one contiguous string,
so any re-wording, and any re-composition of the same facts from the structure
beside it, fails. The client's own line (`skill \`analyze\` (project) is over
this route's budget:`) carries no figure at all, which is asserted positively:
both the measured pair and the budget pair occur **exactly once** in the prompt
block.

**The one figure that is extracted rather than literal** is the measured pair.
Stage A measures the body *with the system prompt*, so a literal would pin the
harness's prompt size rather than the skill's. It is read back out of the
sentence, checked for shape and for exceeding 4,096 words, and then required to
appear verbatim in the *other* sentences of the same measurement — BR-1's
accepted record and AC-3's decline refusal — which is AC-2 asserted at the
surface instead of at the composer.

**The four ids are pinned as two booleans**, because that is what ADR-1 spells
them as: whether the turn was sent, and whether a file on disk changed. Without
the second, `over_budget_proceed_once` and `over_budget_proceed_and_remedy`
produce the same transcript and two of the four ids would be pinned vacuously.
A fifth row covers the prompt's own `(empty refuses the turn)` parenthetical,
and a stray `y` is asserted to re-ask rather than send.

A **daemon per leg**: two of the four answers write the config file the daemon
was started with, so a leg inheriting another leg's write would be answering
about a different route.

Mutation-checked (LESSON-544): re-wording the rendered sentence in
`session_ui.rs`, and dropping `lead_with_remedy`'s verdict condition in
`permissions.rs`, each redden a named assertion here. The second only reddens
after `cargo build --workspace` — a targeted `-p teton --test pty_e2e` run tests
a stale daemon binary and the mutation looked survived.

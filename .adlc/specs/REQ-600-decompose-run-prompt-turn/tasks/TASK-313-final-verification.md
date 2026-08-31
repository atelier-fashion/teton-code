---
id: TASK-313
title: "Final verification, with every figure carrying its rule"
status: complete
parent: REQ-600
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-312]
---

## Description

AC-5 and AC-7. Run every criterion end to end and record the measured figures
with their counting rules.

## Files to Create/Modify

- `.adlc/specs/REQ-600-decompose-run-prompt-turn/requirement.md` — tick each AC
  with evidence, or record it unmet with a reason
- `.adlc/specs/REQ-600-decompose-run-prompt-turn/pipeline-state.json`
- No source files: this task measures and records

## Acceptance Criteria

- [ ] `cargo test --workspace --no-fail-fast` green, output captured and
      **grepped for `FAILED`** rather than trusting a summed count.
- [ ] `cargo clippy --workspace --all-targets` clean under `deny`;
      `cargo fmt --check` clean.
- [ ] AC-5: the REQ-598 event-ordering fixture replays identically, unregenerated.
- [ ] Every figure states its counting rule. `run_prompt_turn`'s length,
      `impl DaemonRuntime`'s production count, and both nesting figures for
      `run_session_turn_with_pressure_policy` each appear with the rule that
      produced them. This REQ line has produced five wrong answers to one
      question by pairing a count with the wrong rule.
- [ ] **AC-7: each commit is independently green — meaning every required check
      on that commit reported success, not that it was not cancelled.** CI sets
      `concurrency: group: ci-${{ github.ref }}, cancel-in-progress: true`
      (`ci.yml:10-12`), so pushing the next step cancels the previous commit's
      still-running `macos-latest` job. Each step is therefore pushed and
      allowed to finish before the next is pushed. If any step ends without
      macOS evidence, AC-7 is recorded **NOT MET** with the cause named —
      REQ-599's identical criterion was ticked without that check and the gap
      was found two REQs later.
- [ ] Every AC ticked with evidence or explicitly called out as unmet.
- [ ] Before the PR is opened, **grep the branch for every figure discarded
      during the work.** If a discarded number still appears anywhere — in a
      task file, an ADR, a test's doc comment — someone is still trusting it
      (LESSON-597).

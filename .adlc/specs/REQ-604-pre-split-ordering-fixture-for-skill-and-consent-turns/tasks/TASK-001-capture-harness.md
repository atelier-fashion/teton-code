---
id: TASK-001
title: "Build the capture harness at 17c39ec and record both sequences"
status: complete
parent: REQ-604
repo: teton-code
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Goal

Record the two missing event sequences at `17c39ec`, in a detached worktree
outside the repo. Nothing from this task is committed to the branch.

## Files to Create/Modify

- `<scratch>/capture-17c39ec/crates/tetond/src/runtime.rs` — a temporary
  `mod req604_capture` inside `mod conversation_carry` (throwaway; never
  committed)

## Acceptance Criteria

- [x] The skill scenario drives a **user-authored** skill through
      `run_prompt_turn` with a `SkillInvocation`, and the run is proven to have
      expanded (the skill body marker reaches the engine).
- [x] The consent scenario drives a scripted `shell` tool call, waits for the
      `permission_request`, resolves it `allow_once`, and joins the turn — no
      wall-clock sleeps (LESSON-450).
- [x] Each scenario is run **at least 20 times** and the raw per-run sequences
      recorded, so entries whose position is unstable are identified
      empirically rather than by reading the code (ADR-4, LESSON-591).
- [x] The raw sequences are written out verbatim for TASK-002 to consume.

## Notes

Driver code must match what TASK-003 uses at tip, since `run_prompt_turn`'s
signature is identical at both commits (ADR-2). A sequence difference must be
attributable to the subject, not to the observer.

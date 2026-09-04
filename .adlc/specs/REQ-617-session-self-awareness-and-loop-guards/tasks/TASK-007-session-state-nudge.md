---
id: TASK-007
title: "The deterministic session-state nudge: when the reply cannot answer, the harness names the command"
status: complete
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: ["TASK-001"]
---

## Description

BR-3 and AC-1(a), per ADR-6. The guide's sentence is the data half and stays;
the guarantee is a deterministic line appended by the surface, reusing REQ-579
ADR-9's shipped hand-off-nudge shape rather than inventing a second one.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — the predicate and the line, beside the
  existing hand-off nudge.

## Acceptance Criteria

- [ ] A reply to a session-state question that omits the command earns one `>>`
      line naming the command and saying only the user runs it.
- [ ] A reply that names the command **and** recites an unusable discovery path
      (a config-file read, a repository search) still earns the line — REQ-579's
      dormancy hole, not reopened.
- [ ] A reply that names the command and nothing else earns **nothing**. This is
      the benign path and it is the one that matters: a nudge that always fires
      is noise, and noise is how the previous one nearly died.
- [ ] Both halves of the predicate read the same backtick-stripped text, asserted
      by the four-spelling loop REQ-579's verify pass established.
- [ ] The command named comes from `SESSION_COMMANDS` (TASK-001), not a literal.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-3 | test-case | `crates/teton/src/session_ui.rs::tests::a_session_state_reply_that_cannot_answer_earns_the_line` | yes |
| AC-1 | test-case | `crates/teton/src/session_ui.rs::tests::a_session_state_reply_that_cannot_answer_earns_the_line` | yes |

## Technical Notes

AC-1(b) — the live trial on the shipped local model — is **not** in this task's
scope and is not runnable in the pipeline (no `llama` feature build, no
downloaded weights). It is recorded in the REQ's verification notes as deferred,
exactly as REQ-616 AC-12 is.

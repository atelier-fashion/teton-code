---
id: TASK-396
title: "The standing pin line and the doctor surfaces — the user finds out the session was pinned"
status: draft
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-393]
---

## Description

BR-7's announcement, rendered by the client from `session_pinned` whether or
not `/verbose` is on, plus the two doctor surfaces that answer the question
later. Also rewords `taint_pin_line`, whose current sentence becomes false
for a liftable pin.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — render `session_pinned` as one standing line under the prompt
- `crates/teton/src/client.rs` — the event reaches the renderer regardless of verbose
- `crates/tetond/src/runtime/taint.rs` — `taint_pin_line` composed from the cause: a liftable arm and a permanent arm
- `crates/teton/src/slash.rs` — `/doctor` reports the live session's pin state
- `crates/tetond/src/runtime/mod.rs` — the doctor payload carries pinned / cause / liftable

## Acceptance Criteria

- [ ] The first pin of a session prints **one** line under the prompt naming the cause, the tier now serving the session with its token budget, and either the remedy or `no remedy: a protected file was read`
- [ ] The line prints with `/verbose` off — it is not a verbose-gated notice
- [ ] A second pin in the same session prints nothing (the transition is what is announced, matching `SessionTaint::mark`'s existing contract)
- [ ] `taint_pin_line`'s permanent arm keeps today's "for the rest of its life" wording; the liftable arm does **not** claim permanence and names `/shell allow`
- [ ] `teton doctor` and `/doctor` show, for a live session, whether it is pinned, the cause, and whether a lift exists (AC-10)
- [ ] Benign path: an unpinned session prints no standing line and its doctor output says not pinned

- [ ] The `#[allow(dead_code)]` on `shell_provenance::Verdict::reason` (added in TASK-392, naming this task) is **removed** — the standing pin line renders it

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/teton/src/session_ui.rs::the_pin_line_prints_once_with_verbose_off` | yes |
| AC-10 | test-case | `crates/teton/src/slash.rs::doctor_reports_pin_state_cause_and_whether_a_lift_exists` | yes |

## Technical Notes

- One composer, two arms (LESSON-557, "compose the sentence where the facts
  are"). Two hand-written sentences in two places is how the permanence claim
  drifts back into the liftable arm.
- The existing stderr `taint_pin_line` call sites (`carry.rs:511`,
  `turn.rs:1637`, `taint.rs:382`) are the daemon's own log, which the user
  does not see — that is why the 2026-09-04 session's user never learned of
  the pin. The new line is a **client** render of the event; do not mistake
  the stderr line for having discharged BR-7.
- `duty.rs`'s doc table already records that nothing in the suite captures
  the daemon's stderr, so a call site that stopped gating on the transition
  would go unnoticed. The new client line is testable — assert the count.

---
id: TASK-008
title: "The AC-10 replay: the recorded 26-call turn completes in at most 9 dispatches"
status: draft
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: ["TASK-004"]
---

## Description

AC-10, the end-to-end claim the other tasks add up to. Per the spec's validation
correction the fixture is hand-authored from the call multiset the REQ's own
Description records, because the transcript file is outside any tree a tool may
read (REQ-611 ADR-7).

## Files to Create/Modify

- `crates/tetond/tests/repeat_refusal.rs` — the replay case.

## Acceptance Criteria

- [ ] The fixture's own total is asserted to be **26** before the replay runs, so
      the baseline cannot drift from the number AC-10 names.
- [ ] Replaying it against a stub model that re-emits the recorded calls
      dispatches at most **9**.
- [ ] The test's doc comment says plainly that a hand-authored multiset is weaker
      evidence than the recorded file, and why the file is unavailable.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-10 | test-case | `crates/tetond/tests/repeat_refusal.rs::the_recorded_twenty_six_call_turn_replays_in_nine` | no |

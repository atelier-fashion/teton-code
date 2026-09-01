---
id: TASK-004
title: "Replay and, if red, disposition per AC-3"
status: complete
parent: REQ-604
repo: teton-code
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-003]
---

## Files to Create/Modify

Outcome-dependent by nature, and that is the point of AC-3 rather than a gap in
this task:

- **replays clean** — no file changes; the evidence is recorded in this task
  and in the wrapup.
- **regression** — the offending source file under `crates/tetond/src/`; the
  fixture is *not* touched.
- **intended change** — the fixture header only
  (`crates/tetond/tests/fixtures/req604_*.txt`), carrying the named REQ, the
  criterion that authorised it, and the delta.

## Acceptance Criteria

- [x] **Both replayed, first run, unmodified.** `cargo test -p tetond --lib
      req604_event_order` — 4 passed, 0 failed. The captured sequences were
      written from the `17c39ec` capture and were never edited to make the
      comparison pass.
- [x] **AC-3's condition did not arise, so its machinery was not exercised.**
      Recorded as N/A rather than ticked: no delta appeared, so there was
      nothing to disposition. Had one appeared, ADR-7 applies exactly — **regression** (fix the
      code, fixture stands) or **intended change** (name the REQ between
      `17c39ec` and tip, name the criterion that authorised it, record the
      delta in the fixture header, pin as *captured sequence plus stated
      delta*). Default when neither can be shown: **regression** (AC-3).
- [x] Regeneration at tip was not used at any point. The only writes to either
      fixture were the initial capture output and the transposition mutation,
      which was reverted.

## What the green result actually means

Both sequences surviving is a substantive finding, not a null result: the four
refactors between `17c39ec` and tip — REQ-598 (`TurnContext`), REQ-599
(decomposing `runtime.rs`), REQ-600 (the eight-stage split), REQ-602 (post-split
cleanup) — each claimed to be behaviour-preserving, and each claim was
previously evidenced on the *plain typed turn* only. The skill-expansion and
consent orderings are now evidenced too, against a pre-split oracle.

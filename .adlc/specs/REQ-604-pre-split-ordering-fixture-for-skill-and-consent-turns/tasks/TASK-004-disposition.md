---
id: TASK-004
title: "Replay and, if red, disposition per AC-3"
status: pending
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

- [ ] If both replay: record that, with the evidence.
- [ ] If either does not replay, apply ADR-7 exactly — **regression** (fix the
      code, fixture stands) or **intended change** (name the REQ between
      `17c39ec` and tip, name the criterion that authorised it, record the
      delta in the fixture header, pin as *captured sequence plus stated
      delta*). Default when neither can be shown: **regression** (AC-3).
- [ ] Regeneration at tip is not used under any circumstances.

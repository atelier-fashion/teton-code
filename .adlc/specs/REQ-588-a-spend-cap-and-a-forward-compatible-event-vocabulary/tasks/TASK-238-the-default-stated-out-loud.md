---
id: TASK-238
title: "what a fresh install does, said out loud"
status: complete
parent: REQ-588
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-236"]
---

## Description

BR-5, AC-5. A ceiling that appears silently is the mirror of the window that was recorded silently.

## Files to Create/Modify

- `crates/tetond/src/harness/docs/*.md` — the `teton_docs` sentence
- `docs/` or the release notes — the fresh-install statement

## Acceptance Criteria

- **AC-5**: a test asserts the `teton_docs` sentence exists rather than trusting it was written
- the sentence states that there is **no ceiling until one is configured**, names the key, and states ADR-2's one-call overshoot — a user reading "$5.00" must know what they are actually promised
- the release-notes half is a wrapup checklist item, recorded as such rather than pretended to be a test

---
id: TASK-263
title: "Record the recovery point before anything is rewritten"
status: draft
parent: REQ-591
created: 2026-08-25
updated: 2026-08-25
dependencies: []
---

## Description

ADR-6 step 1. `feat/REQ-589-over-budget-skill-expansion-offer` is pushed at `9067374`. Before any operation that could lose it, make it recoverable **by name** rather than by reflog archaeology.

## Files to Create/Modify

- `.adlc/specs/REQ-591-project-skill-trust-and-unattended-allowlist/pipeline-state.json` — record the pre-rewrite SHA

## Acceptance Criteria

- [ ] A local tag `req589-pre-carveout` points at the pre-rewrite tip of the 589 branch
- [ ] The same SHA is written into pipeline-state.json, so a context loss cannot lose it
- [ ] `git show req589-pre-carveout` resolves and its tree matches the branch tip exactly
- [ ] The tag is NOT pushed — it is a local recovery point, not a shared ref

## Technical Notes

This task exists because every later task is easier to undo than to redo. Do it first even though it produces nothing visible.

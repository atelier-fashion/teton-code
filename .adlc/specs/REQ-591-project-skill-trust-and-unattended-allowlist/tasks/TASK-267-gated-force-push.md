---
id: TASK-267
title: "Surface the force-push for confirmation — do not perform it"
status: complete
parent: REQ-591
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-265, TASK-266]
---

## Description

ADR-6 steps 4-5. Rewriting a pushed branch is the one step here that can lose work. It is gated deliberately.

## Files to Create/Modify

- nothing — this task produces a report, not a commit

## Acceptance Criteria

- [ ] Both branches verified green LOCALLY first: REQ-591's (TASK-264) and REQ-589's rebuilt (TASK-266)
- [ ] The exact command is surfaced for the owner, using `--force-with-lease` and NEVER `--force`, so a concurrent remote update aborts rather than clobbers
- [ ] The recovery path is stated alongside it: `req589-pre-carveout` (TASK-263) restores the pre-rewrite branch
- [ ] The diff between the old and new 589 branch is summarized — what leaves, what stays, and the test-count delta
- [ ] **The push is NOT performed by this task.** It stops and asks

## Technical Notes

The pipeline's autonomy contract does not extend to rewriting shared history. Every other step in this REQ is additive and reversible; this one is not, and the asymmetry is the reason for the gate.

---
id: TASK-232
title: "the AC-13 runbook"
status: complete
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-231"]
---

## Description

AC-14. `docs/manual-verification.md` gains the AC-13 live A/B, including the macOS note that the first scan may raise the Documents dialog.

AC-13 itself is a **real-model check on the user's machine** and cannot be run from here; the runbook is what makes it reproducible.

## Files to Create/Modify

- `docs/manual-verification.md` — the runbook section

## Acceptance Criteria

- the runbook states both legs (warm registry, then empty registry), the exact prompt, and what to observe at the surface
- it names the macOS Documents dialog as expected on first scan, per BR-3 and A-5
- it records that the model's prose is an observation, not an assertion (LESSON-532)

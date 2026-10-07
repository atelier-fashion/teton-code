---
id: ASSUME-050
title: "The model maps the vendored skills' Agent/subagent_type phrasing onto `agent { tasks }` from the roster sentence and schema alone"
status: unresolved
req: REQ-623
created: 2026-10-07
resolved:
---

## Assumption

The ADLC skills vendored into a project phrase dispatch in Claude Code's
vocabulary — "launch an `Agent`", `subagent_type: reviewer`,
`run_in_background` — while Teton's tool is `agent` with
`{ tasks: [{task, name?, tier?, context?}] }`. REQ-623 assumes a frontier model
bridges that gap from the resident-prompt sentence ("it is what the skills call
the Agent tool") and the tool schema, with no per-skill translation.

## Context

AC-18 (dogfood) depends on it: `/analyze` fanning its auditors out through
`agent`, and `/proceed` passing the Phase-4 dispatch step. The runbook in
`docs/manual-verification.md` is OUTSTANDING; until it runs, nothing has
observed a real model making the mapping. If it does not, the fix is a roster
sentence or a toolkit change — not a schema that mimics Claude Code's.

## Resolution

(unresolved — recorded by the AC-18 run in REQ-623's Validation section)

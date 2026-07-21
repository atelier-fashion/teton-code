---
id: TASK-009
title: "tetond agent harness: tool loop, permissions, local-first sessions"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-004, TASK-006]
---

## Description

The agentic core: tool-use loop (read/edit/glob/grep/shell), permission model
(allowlist + permission-request round-trip to clients), edit verification, and
freeform sessions running end-to-end against the LOCAL model only (D-3 —
remote wiring arrives with TASK-010). Delivers the offline AC-1 path.

## Files to Create/Modify

- `crates/tetond/src/harness/loop.rs` — turn loop: context assembly (with provenance tagging hooks for TASK-007), model call, tool dispatch, result folding
- `crates/tetond/src/harness/tools/` — built-in tools: `read.rs`, `edit.rs` (exact-match replace + verify), `glob.rs`, `grep.rs`, `shell.rs` (timeout, cwd jail)
- `crates/tetond/src/harness/permissions.rs` — per-tool policy (allow/ask/deny), `permission_request` event → client response round-trip, session-scoped grants
- `crates/tetond/src/harness/context.rs` — context window management for small models: aggressive truncation, tool-result summarization via local model
- `crates/tetond/tests/offline_session.rs` — freeform session on mock-local-model: reads a file, edits it, verifies, completes with zero egress

## Acceptance Criteria

- [ ] Offline freeform session completes a read→edit→verify flow with the local engine mock and zero remote calls (AC-1 core)
- [ ] Edit tool rejects non-unique or non-matching replacements; failed edits surface to the model for retry, never silently succeed
- [ ] Denied permission cancels the tool call and informs the model; grant persists for the session only
- [ ] Shell tool enforces timeout and repo-root cwd jail; env is scrubbed of `*_KEY`/`*_TOKEN` vars
- [ ] Loop terminates on max-turns and on model end-of-turn; no unbounded loops under malformed tool calls

## Technical Notes

Design the loop for WEAK models from day one (this is the product thesis):
short loops, small tool set, mandatory verification — the degradation profile
(BR-6) is the harness's native shape, and strong models just get longer
leashes. Tool-call parsing already normalized by teton-providers; local engine
uses the same internal shape.

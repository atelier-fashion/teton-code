---
id: TASK-230
title: "/cd accepts a project name, after the shell's own reading"
status: complete
parent: REQ-584
created: 2026-08-22
updated: 2026-08-22
dependencies: ["TASK-223"]
---

## Description

BR-8, AC-9. `resolve_cwd_argument` widens to take a borrowed candidate slice (ADR-10) — path reading first, registry second, one composer for the two-reading refusal.

## Files to Create/Modify

- `crates/teton-core/src/session_root.rs` — widen `resolve_cwd_argument`; extend `CwdRefusal`
- `crates/teton/src/slash.rs` — pass the candidates through

## Acceptance Criteria

- AC-9 in full: a unique name moves; `./src` beats a known project named `src`; two matches print both candidates and move nowhere; no match yields the **two-reading** refusal naming both
- REQ-583's `CwdGrammarRow` table is re-run **unchanged** and still passes through the same entry point — the point of widening rather than wrapping
- `--cwd` keeps path semantics only (OQ-3/ADR-9)

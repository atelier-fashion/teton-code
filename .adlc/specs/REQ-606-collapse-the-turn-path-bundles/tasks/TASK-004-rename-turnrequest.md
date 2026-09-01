---
id: TASK-004
title: "Rename the TurnRequest bundle to PromptRequest to stop it shadowing the provider type"
status: pending
parent: REQ-606
created: 2026-09-01
updated: 2026-09-01
dependencies: [TASK-001]
---

## Description

A defect found while classifying, not a rename for taste. `runtime/turn.rs`
declares `struct TurnRequest<'a>` and opens `use super::*;`; `runtime/mod.rs`
imports `teton_providers::TurnRequest`, the provider-facing request built at
`mod.rs:5629`. A local item shadows a glob import **silently** — no warning —
so inside the turn path the provider type is unreachable by its own name.

Rule A keeps the bundle. This only stops the collision, and `PromptRequest` is
the more accurate name: a "turn request" is what goes to a provider.

**LESSON-599 applies.** Bound the rename to code tokens and diff the prose
separately — a word-boundary regex reaches string literals and comments, which
is the one place the compiler and the suite cannot notice.

## Files to Modify

- `crates/tetond/src/runtime/turn.rs`

## Acceptance Criteria

- [ ] The bundle is `PromptRequest`; no `TurnRequest` shadow remains in `turn.rs`
- [ ] `git diff | grep '^[-+].*//'` reviewed — no unintended prose rewrites
- [ ] No guard test names the old bundle (verified, not assumed)
- [ ] Suite green; clippy 0 under `deny`; fmt clean

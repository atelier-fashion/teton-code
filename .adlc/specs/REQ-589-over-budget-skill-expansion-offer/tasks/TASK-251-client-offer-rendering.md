---
id: TASK-251
title: "Client: render and answer the over-budget offer"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-241, TASK-243]
---

## Description

BR-4 + ADR-2. `PermissionSubject` is matched exhaustively client-side, so the new variant forces every arm. The no-terminal and unrecognized paths must refuse, never proceed.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — `consent_gate` (2836), `resolve_permission` (2891), `render_consent_subject` (~3042), summary/echo renderers (~3153)

## Acceptance Criteria

- [ ] `consent_gate`'s `RefuseNoTerminal` arm fires BEFORE `prompter.ask` reads a line, so a piped answer for a later prompt cannot be swallowed as this one
- [ ] The rendered offer quotes the same figures the measurement produced and names the bound verbatim (AC-2)
- [ ] The four options render as a single-select; the remedy options are absent when the bound has no remedy
- [ ] A project-sourced skill's name renders under the distinguishing treatment project skills already get, never as bare harness vocabulary (ASSUME-018)
- [ ] Reuse `bound_clause`/`bound_words` (1944, 1975) and `budget_clause` (3282) — no second vocabulary for the same fact (LESSON-456)
- [ ] A client-side test asserts the no-terminal path returns `Refused { reason: NoTerminal }` and never `Cancelled`, and that it fires before any read of stdin (BR-4)
- [ ] A rendering test pins the offer's rendered text for each of the five bounds, driven from a constructed `PermissionRequest` — and a producer-side test proves the daemon actually emits that subject (LESSON-544: a struct-literal test alone leaves the producer unguarded)
- [ ] Removing the `SkillOverBudget` arm fails compilation, demonstrating the exhaustive-match forcing function (ADR-2)

## Technical Notes

An older client hits the `#[serde(other)]` Unrecognized arm and refuses rather than mis-rendering — BR-4-compatible; leave it intact.

---
id: TASK-284
title: "The manual runbook, the architecture record, and the release note"
status: complete
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: [TASK-282, TASK-283]
---

## Description

The half no test can make: a human looking at the screen and saying whether it is better. Plus the
two records this REQ owes. Covers AC-13.

## Files to Create/Modify

- `docs/manual-verification.md` — the REQ-592 runbook section.
- `.adlc/context/architecture.md` — the rendering-layer paragraph (architecture.md's "Proposed
  addition").
- `CHANGELOG.md` — one entry covering both halves.

## Acceptance Criteria

- [ ] AC-13: a runbook section in the file's established shape — `## What this proves that CI does
      not`, `## Prerequisites`, `## Procedure` with lettered legs, `## Sign-off`.
- [ ] Leg (a): ask a real session for a security audit of this repository at **100 columns**;
      record whether tables and prose are legible.
- [ ] Leg (b): the same at **200 columns**.
- [ ] Leg (c): `NO_COLOR=1` at 100 columns — wrapping present, no escapes.
- [ ] Leg (d): the same prompt with stdout redirected to a file — output is unrendered markdown
      (BR-7 by eye, complementing AC-7).
- [ ] The runbook carries the **prompt**; `fixtures/audit-2026-08-26.md` is named as the *before*,
      with a note that it holds the reply and not the prompt.
- [ ] `.adlc/context/architecture.md` records: markdown rendering is terminal-only and opt-in at
      construction; layout is pure and width-parameterised in `markdown.rs`; styling is authored
      inside the sanitizer from a fixed table; `client.rs`'s pump solely owns `end_block()`.

## Technical Notes

This is where the two halves are judged together for the first time — TASK-282 changes what the
model writes, TASK-277..281 change how it is drawn, and only a live session exercises the pair.
Every automated AC scripts its replies precisely so it does *not* depend on model behaviour, which
is exactly why this leg cannot be dropped.

Record the outcome honestly, including a partial one. If the clause's wording does not move model
behaviour, that is a finding about BR-1 worth writing down, not a reason to re-run until it looks
good — prompt-adjacent behaviour is chaotic under byte-level changes (BUG-168), and REQ-577's
`teton_docs` docstring needed live re-tuning for the same reason.

The architecture-record paragraph belongs beside the existing `Surface`/`Prompter` seam entry
(~line 643), not in a new section — this REQ extends that seam rather than adding one.

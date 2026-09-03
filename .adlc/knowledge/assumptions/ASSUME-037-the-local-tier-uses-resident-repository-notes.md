---
id: ASSUME-037
title: "The local tier uses resident repository notes instead of rediscovering the tree with tools"
status: unresolved
req: REQ-612
created: 2026-09-03
resolved:
---

## Assumption

REQ-612's spec ASSUME-1: a small local model given an 8 KiB `TETON.md` block at
the end of its system prompt answers layout and build questions from the notes
rather than reaching for `glob`/`grep`. LESSON-532 (a small model transfers
*data* more reliably than directives) and LESSON-543 (a resident fact beats a
tool the model may not call) are the evidence the feature was designed on; the
environment line (REQ-583) is the precedent that worked.

## Context

The block costs about an eighth of the local byte budget on every call of every
turn. If the local tier ignores the notes, that cost buys nothing on the tier
the feature most wanted to help, and the remedy is `[context] repo_file = false`
or a different frame — not a larger cap. AC-13 is the check: a fresh session in
this repository with a `TETON.md` describing the crate layout, first prompt
"where does the system prompt get built?", counting `glob`/`grep` calls with and
without the file on the local tier. Its runbook leg is in
`docs/manual-verification.md`, status `OUTSTANDING`.

## Resolution

Unresolved. This repository has no `TETON.md` on `main`; REQ-613 (generation
when absent) is the planned way one lands. Run AC-13 after REQ-613 ships, or
author a `TETON.md` by hand and run it sooner. Record the counts here.

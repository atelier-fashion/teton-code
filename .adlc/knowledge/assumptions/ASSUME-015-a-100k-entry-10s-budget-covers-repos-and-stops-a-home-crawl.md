---
id: ASSUME-015
title: "A 100,000-entry / 10 s walk budget covers every realistic repository and stops a home-folder crawl well inside a shell timeout"
status: validated
req: REQ-583
created: 2026-08-19
resolved: 2026-08-19
---

## Assumption

REQ-583 A-2: `WalkBudget::default()` at 100_000 entries and 10 s (clock read
after every entry) is large enough that a single repository is never
truncated in practice, and small enough that a `~`- or `/`-rooted walk ends
long before the shell tool's 30 s timeout.

## Context

The numbers were an architecture decision (spec OQ-4). They bound every
`glob`/`grep` from any root; a truncated walk is reported (`... (stopped after
…)`), never silent, so a wrong number degrades loudly.

## Resolution

Validated on one machine (2026-08-18/19): this workspace is ~2,500 entries
outside the skip set; TASK-180's live A/B against the real local model from
`~` saw `glob **/teton*` end by budget and return within seconds (the home has
>100k entries under the skip rules); no repository walk in the suite is
truncated. Re-examine if a monorepo user reports the stopped line on ordinary
patterns, or if the per-entry clock proves too coarse on network mounts.

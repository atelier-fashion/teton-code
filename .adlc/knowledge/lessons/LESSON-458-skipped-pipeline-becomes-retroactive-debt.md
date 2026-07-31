---
id: LESSON-458
title: "A skipped pipeline resurfaces as retroactive debt"
component: "adlc/process"
domain: "adlc"
stack: ["rust", "ci"]
concerns: ["process", "knowledge-capture"]
tags: ["pipeline-skip", "backfill", "fmt-gate", "verify-phase", "chat-scoped-work"]
req: REQ-549
created: 2026-07-31
updated: 2026-07-31
---

## What Happened

REQ-549 (daemon rename + startup UX) was implemented straight from chat scope:
no spec, no architect phase, no /reflect, no multi-agent /review, PR opened
and merged same-session. Two costs materialized immediately. First, CI failed
on `cargo fmt` — the rename lengthened two lines past width — which the
skipped verify phase runs locally; the fix cost an extra commit and a full CI
round-trip on both platforms. Second, the design decisions that mattered
(bin-target-only rename, stable runtime filenames as an upgrade invariant,
frame-the-entry-not-the-dialogue) lived only in code comments and a PR body
until the user asked "did you follow the ADLC process?" — after which the
requirement, pipeline state, and lessons had to be reconstructed from memory
and merged history, at higher cost and lower fidelity than capturing them
in-phase.

## Lesson

"Small, well-understood change" is exactly the judgment the pipeline exists to
check (ETHOS #5). If work genuinely warrants skipping ceremony, that is a
decision to surface to the user *before* implementing — not one to make
silently and backfill later. At minimum, chat-scoped work must still run the
local verify gates (`cargo fmt --check`, clippy, tests) before pushing, and
must leave a knowledge trail for any decision a future session could plausibly
reverse for lack of context.

## Why It Matters

The backfilled record is honest but reconstructed: a phased run would have
caught the fmt failure pre-push, might have surfaced the Keychain-ACL side
effect at spec time instead of post-merge, and would have captured
[[LESSON-457]] while the reasoning was fresh. The invisible cost is worse than
the visible one: undocumented invariants (stable socket filenames) are exactly
what a future "finish the rename" session would break first.

## Applies When

- A request arrives conversationally ("bundle it with this") and feels too
  small for `/spec` → `/proceed` — say so explicitly and let the user choose.
- Any direct-to-PR work: the verify phase's local gates still apply even when
  the phases around them are skipped.
- Writing retroactive artifacts: mark them as backfilled (status, dates, and
  what was NOT run) so the record never masquerades as a phased run.

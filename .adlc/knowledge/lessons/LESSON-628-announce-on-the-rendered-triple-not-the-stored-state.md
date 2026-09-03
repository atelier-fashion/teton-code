---
id: LESSON-628
title: "Announce on what was rendered, not on what was stored — and measure a resident block's cost, never add the cap to a number"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon", "cli"]
concerns: ["developer-experience", "cost", "reliability"]
tags: ["repo-context", "route-aware", "silent-truncation", "events", "resident-prompt", "ceiling", "headroom", "measure-not-derive", "verify-loop"]
req: REQ-612
created: 2026-09-03
updated: 2026-09-03
---

## What Happened

Two findings in one REQ, the same shape.

First: the repository-notes block is loaded once per session and classified
`Loaded` or `Truncated` against the 8 KiB maximum, but rendered per turn at the
*route's* cap. The `repo_context_state` event fired only when the stored state
changed, carried the load-time word beside a route-time `truncated` flag, and
the CLI branched on the word. On a route with a smaller cap a 6,000-byte file
was cut to 4,096 with no line at all — precisely what BR-3 ("nothing is clamped
in silence") forbade. The fix keys the announcement on the rendered triple
`(state, truncated, resident_bytes)` compared with the last *published* triple,
derives the wire word from the rendered block, and makes the client read the
flag before the word. A mid-turn reroute then needed the same gate, because it
re-renders too.

Second: the architecture predicted the resident-prompt ceiling would move from
14 to 22 KiB — the old ceiling plus the 8,192-byte cap. Measured, the block
costs 8,603 bytes (331 of frame, 80 of guide sentence), and 22 KiB left the
widest prompt 282 bytes short of the floor. The implementer moved it to 23 KiB
by measurement and reported the arithmetic the prediction had also missed: the
redact chunk cap rose 3 → 4 and the scannable bound *rose* rather than shrank.

## Lesson

**When a rendering depends on an input the stored state does not carry, the
announcement must be keyed on the rendering.** Publish on the change of the
tuple the user actually sees; derive the wire word from what was rendered; and
audit every path that renders — including the reroute that re-renders — for the
same gate. **And a resident block's cost is its frame and every sentence it
forces, not its cap.** Predict nothing by adding a cap to a ceiling; build the
worst case, run the sweep, read the number, and re-derive whatever the ceiling
feeds (LESSON-593, LESSON-597), because the direction of a derived figure can
surprise you.

## Why It Matters

Both defects passed a green suite: the silent truncation because the tests
keyed on the same stored word the code did, the ceiling because a predicted
number was written into an ADR before anyone built the block. The first was
found by two independent reviewers reading the event's two fields side by
side; the second by the implementer refusing to pin an arithmetic value. Each
is the LESSON-486 shape — a doc comment or ADR asserting a property the code
does not have.

## Applies When

Any per-route or per-turn rendering of session-level state (budgets, caps,
elisions, truncation markers); any event whose payload mixes a stored
classification with a computed one; any resident-prompt addition (measure
headroom first, LESSON-543); any ADR that names a figure before the artifact
that produces it exists.

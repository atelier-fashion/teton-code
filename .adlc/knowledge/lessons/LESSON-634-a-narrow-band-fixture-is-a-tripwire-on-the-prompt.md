---
id: LESSON-634
title: "A narrow-band fixture is a tripwire on the resident prompt; re-centre it, do not nudge it"
component: "tetond/harness"
domain: "testing"
stack: ["rust"]
concerns: ["maintainability", "developer-experience"]
tags: ["fixture", "budget", "band", "prompt-margin", "regression", "context-window"]
req: REQ-615
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

`skill_over_budget_offer`'s `window / fits` cell needs a measured expansion in
the band `(window − reservation) × 2 < measured ≤ window × 2` — 2,048 bytes wide.
Its body sat 221 bytes below the ceiling. REQ-615 added 278 bytes to the
resident system prompt and the cell reported `ExceedsWindow`.

REQ-612 had already moved this same cell for the same reason, and its comment
recorded the move. Both REQs were about something else entirely; neither
intended to touch a budget fixture.

The fix was not to subtract 278. The framing overhead between the body and
`measured` was read off the failing run (8,357 bytes), the band's admissible
body range was computed from it (49,595 → 51,643), and the body was moved to
50,600 — roughly its middle, with about a kilobyte of clearance on each side.

## Lesson

When a fixture must land inside a narrow band, **centre it and record the band's
arithmetic**, because anything that grows the system prompt will move it. Nudging
it just past the failure leaves the next REQ to rediscover the same thing, and
the failure it produces (`ExceedsWindow` where `FitsWindow` was expected) says
nothing about prompt size — the author has to work out that a budget cell is a
tripwire on a byte count they did not know they had changed.

Measure the overhead rather than guessing it: one debug print of
`measured`/`budget` from the failing case gives the band exactly.

## Why It Matters

Two REQs in a row paid for this, and both diagnosed it from scratch. The signal
is real and worth keeping — a prompt that grew is a fact somebody should notice —
but it should cost one reading of a comment, not one investigation.

## Applies When

Any test fixture tuned to sit inside a computed band, especially one whose bounds
depend on the system prompt, a context budget, or another figure that unrelated
work routinely moves.

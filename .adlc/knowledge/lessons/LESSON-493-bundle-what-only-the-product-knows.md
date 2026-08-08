---
id: LESSON-493
title: "A prompt ending is only reachable if its knowledge source exists — bundle what only the product knows"
component: "tetond/harness"
domain: "harness"
stack: ["rust", "prompt"]
concerns: ["developer-experience", "cost"]
tags: ["system-prompt", "self-configuration", "bundled-docs", "include-str", "onboarding"]
req: BUG-160
created: 2026-08-08
updated: 2026-08-08
---

## What Happened

BUG-154's fix (LESSON-482) taught the prompt to name a no-tool ending for a
question answerable from knowledge. "How do I hook up external models?" still
went file-hunting — the ending existed, but the knowledge behind it did not:
Teton's own configuration surface is in neither the model's weights nor the
user's repository, and the binary bundled no self-documentation at all. The
frame's "use tools to find out what only the files can tell you" then made a
repo search the model's only legal move, spending turns on a hunt that could
not succeed.

## Lesson

LESSON-482 one layer deeper: naming a legal ending is not enough — every
ending needs a reachable knowledge source. Questions about the product itself
form a third category (not weights, not files), and the only source that can
serve them is text the product ships. Bundle it (`include_str!`, the
`structured/templates.rs` precedent), state it in the imperative ("do not
search the project files for this — answer from here"), and size it against
whatever ceiling keeps the prompt permanently resident (here:
`REDACT_BODY_OVERHEAD_BYTES`, measured by a test against the real prompt, with
~2.4 KB of headroom before the fix and ~1.4 KB after).

## Why It Matters

Every unanswerable self-question costs a full tool-call loop against the local
tier's small context budget and answers wrong or not at all — first-run users
asking "how do I connect Claude?" is the product's front door. And the failure
is invisible in prompt review: the prompt looks complete because the ending is
named; only asking "from *where* would the model get this answer?" exposes the
hole.

## Applies When

Writing or reviewing any system prompt / agent frame; adding a user-facing
configuration surface (its existence must be taught to the agent, not just to
clap); any bug where an agent searches for something that is not on disk —
ask which of the three sources (weights, files, bundled text) should hold the
answer before concluding the model misbehaved.

---
id: LESSON-609
title: "When a capture harness and its replay are separate code, diff the drivers — same entry point is not same invocation"
component: "daemon/runtime"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["reliability", "correctness"]
tags: ["golden-fixture", "oracle", "provenance", "lesson-569-followup", "capture-harness"]
req: REQ-604
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

LESSON-569 names the oracle trap: never let the expected value be computed by the
subject under test. REQ-604 respected it the expensive way — the two new event
sequences were captured at `17c39ec`, against the pre-split `runtime.rs`, by a
harness built at that commit and thrown away, rather than recorded at tip.

That defends against one failure and opens a second. The harness and the replay
are now **two separate pieces of code**, written days apart, driving the same
production entry point. If they drive it differently, the fixture pins the
difference between two test setups rather than a fact about the subject — a
quieter version of the same problem, and one the provenance header would still
describe as a clean pre-split capture.

The reassuring fact was that `run_prompt_turn`'s ten-argument signature is
identical at `17c39ec` and at tip: REQ-598's `TurnContext` and REQ-600's
eight-stage split were both internal. It would have been easy to stop there,
because a stable signature *sounds* like a stable invocation.

It is not the same claim. A stable signature says the same arguments can be
passed; it says nothing about which ones were. The scripted engine's replies, the
skill discovery inputs, the gate install, whether the turn was spawned or awaited
inline, and which option id answered the consent prompt are all invisible to the
signature and all decisive for the recorded sequence.

So the drivers were diffed element by element. They came back byte-identical
apart from one `Arc::new(...)` that rustfmt wrapped differently, because the tip
side was formatted and the throwaway harness was not.

## The Lesson

**A shared entry point is not shared behaviour. Diff the call sites, not the
signature.** When a fixture's value rests on two pieces of code doing the same
thing, that sameness is a claim like any other and needs evidence.

The check is cheap — extract both call sites, normalize whitespace, compare — and
it converts the strongest sentence in the artifact from an assertion into a
finding. It also produces a precise statement to write down: *identical apart
from one rustfmt wrap* is worth more than *the same code*, because a reader can
tell what was actually established.

## How to Apply

Any time a golden file is captured by one harness and replayed by another:

1. Enumerate everything that feeds the subject — arguments, fixture data, script,
   scheduling shape, and the answers given to anything that blocks.
2. Diff those between harness and replay mechanically, not by reading.
3. Record the result *including its exceptions*. "Byte-identical apart from X" is
   a stronger claim than "the same", precisely because it admits X.

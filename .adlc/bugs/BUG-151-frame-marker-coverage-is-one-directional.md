---
id: BUG-151
title: "The frame-marker coverage invariant only holds in one direction"
status: open
severity: medium
created: 2026-08-04
updated: 2026-08-04
component: "tetond/harness"
domain: "harness"
stack: ["rust"]
concerns: ["security", "test-coverage"]
tags: ["prompt-injection", "frame-forgery", "invariant", "drift-guard", "test-gap"]
---

## Description

BUG-148 added `the_input_alphabet_covers_every_output_marker` as the anti-drift guard for
the harness's prompt-injection posture (ADR-009): what the model must not *emit* is what
untrusted content must not *introduce*. The test iterates the **output** marker sets and
asserts each has an input guard.

It does not assert the reverse. A marker present on the **input** side but missing from the
output sets passes silently — and that is exactly the shape BUG-149 had: `<mcp-tool-result`
was in `UNTRUSTED_ENVELOPE_TAGS` (defused on input by BUG-148) but was not in
`FLAT_ANCHORED_MARKERS` / `CHATML_ANCHORED_MARKERS`, so a model could still emit a
fabricated MCP envelope uncut. BUG-149 was found by reading the code, not by the suite.

ADR-009 records this as a standing caveat. This bug closes it.

## Reproduction Steps

Against `main` at `39ff1f8` (BUG-148 merged, BUG-149 not yet):

1. `<mcp-tool-result` is present in `render.rs::UNTRUSTED_ENVELOPE_TAGS`
2. `<mcp-tool-result` is absent from both anchored marker sets in `reply.rs`
3. `cargo test -p tetond --lib harness::render` — passes

Both drift guards are silent on that state.

## Expected Behavior

A marker defused on the input side but absent from the output fabrication markers is a
build failure, not something a human has to notice.

## Actual Behavior

Both guards pass. `the_input_alphabet_covers_every_output_marker` never iterates the input
alphabet, and `the_two_neutralizers_do_not_overlap` — which *does* iterate
`UNTRUSTED_ENVELOPE_TAGS` — only asserts each marker is claimed by exactly one input layer,
which stays true regardless of what the output side contains.

## Environment

- Platform: all
- Version: `main` @ `934982e`

## Root Cause

`the_input_alphabet_covers_every_output_marker`
(`crates/tetond/src/harness/render.rs`) iterates only the output sets:

```rust
for marker in super::super::reply::FLAT_ANCHORED_MARKERS
    .iter()
    .chain(super::super::reply::CHATML_ANCHORED_MARKERS)
{
    assert!(starts_with_frame_label(marker) || starts_with_envelope_tag(marker), ...);
}
```

The invariant it encodes is `output ⊆ input`. The posture in ADR-009 claims something
stronger and symmetric — "frame is frame in both directions" — which requires
`input ⊆ output` as well.

### The one legitimate asymmetry

The reverse containment is **not** universally true, and a naive symmetric test would fail
today. `UNTRUSTED_ENVELOPE_TAGS` includes the **closing** tags `</tool-result` and
`</mcp-tool-result`, which are deliberately input-only: a model that emits `</tool-result>`
has not forged a tool result, so it is correctly absent from the fabrication markers.
`render.rs` already documents this ("Not a fabrication marker on the output side").

So the correct invariant is: **every *opening* envelope tag must be an output marker**;
closing tags are exempt by construction. Transcript labels need no separate check — they
are already derived from the output sets, so their containment is structural.

## Fix Approach

Add the reverse assertion to the existing test (or a sibling), iterating
`UNTRUSTED_ENVELOPE_TAGS`, skipping the `</` closers, and asserting each opening tag
appears in both anchored marker sets. Verified by construction: the test passes on `main`
today (BUG-149 added `<mcp-tool-result` to both sets) and would have failed at `39ff1f8`,
which is what makes it a real guard rather than a restatement.

## Resolution

(filled after fix)

## Files Changed

(filled after fix)

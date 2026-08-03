---
id: LESSON-475
title: "A marker must be anchored the way the renderer actually writes it — and scoped to what is never legitimate output"
component: "tetond/harness"
domain: "harness"
stack: ["rust"]
concerns: ["correctness", "developer-experience"]
tags: ["markers", "line-anchoring", "scanner", "false-stop", "chatml", "test-fixture-lies"]
req: REQ-554
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

The reply scanner detects fabricated frame by matching markers **at a line
start** — correct for the flat rendering, whose labels (`User:`, `Tool (`) the
harness always writes at line starts, and load-bearing there because ordinary
prose contains `User:` mid-line. Adding ChatML markers, that anchoring was
inherited unexamined. But the ChatML renderer emits `{text}<|im_end|>\n` — the
closing delimiter lands **mid-line**, which is the shape the model reproduces.
The marker could essentially never fire in production. The unit test passed
because it fed `"All done.\n"` then `"<|im_end|>\n"`, inserting a newline the
renderer never produces: the fixture asserted the narrow claim ("stops at a
*line-leading* delimiter") while reading like the broad one.

The mirror error appeared in the fix: the duty-path scan-cut was given the
format's *full* marker set, so a summarizer summarizing a transcript — or this
repo's own source — would hit `Assistant:` at a line start and have its correct
summary silently truncated.

## Lesson

Two questions per marker, answered separately:

1. **How is it written?** Derive the anchoring from the renderer, not from the
   marker set it joins. A token that is self-delimiting (`<|im_start|>`) has
   turn meaning at *any* offset; a bare word label (`User:`) needs the line
   anchor to avoid false stops. Mixing them into one anchored list gets one of
   the two wrong.
2. **Is it ever legitimate output?** Scope each marker to the path where it
   cannot be innocent. Control tokens are never legitimate anywhere, so they
   belong in every set. Prose-shaped labels are legitimate in a summary of a
   transcript, so a summarizer must not watch for them.

And the fixture rule that would have caught both: **build the test input the
way the renderer builds it.** A hand-written fixture that inserts a separator
the production path never emits is a test asserting a claim adjacent to the
one it appears to assert.

## Why It Matters

An under-anchored marker is a containment control that reports success while
never firing — worse than absent, because it is believed. An over-scoped
marker silently truncates correct output with no elision notice, which is the
failure mode users cannot even report clearly ("the summary just stops").
Neither shows up in a green suite when the fixtures were written from the same
mental model as the code.

## Applies When

Adding a marker/sentinel/delimiter to an existing detection set; writing
detection for a format whose *emitter* lives in another module (read the
emitter, not just the spec); reusing an output-side scanner on a second
consumer (a duty, a summarizer, a secondary parse) — re-ask which markers can
be innocent there.

## Related

- [[LESSON-472]] — the containment this refines.
- [[LESSON-474]] — the input-side twin of the same frame/content confusion.

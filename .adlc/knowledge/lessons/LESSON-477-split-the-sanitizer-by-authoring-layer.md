---
id: LESSON-477
title: "Harness-authored frame that lives inside content is indistinguishable from forged frame — split the sanitizer by the layer that writes each marker"
component: "tetond/harness"
domain: "harness"
stack: ["rust"]
concerns: ["security"]
tags: ["prompt-injection", "frame-forgery", "choke-point", "sanitizer", "layering", "trust-boundary"]
bug: BUG-148
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

BUG-148 was the text-level twin of REQ-554's tokenizer-level injection: the flat
rendering interpolated untrusted content between line-anchored `User:` /
`Assistant:` / `Tool (name):` labels with no escaping, so a repo file body could
render as a byte-perfect forged turn pair. LESSON-474 had already established
where the choke point goes — below the format branch, at the layer that parses —
so the fix looked like one function applied to every block at assembly.

That single pass broke immediately, on a *pre-existing* test. The
`<tool-result trust="untrusted">` envelope is harness-authored frame, but
`frame_untrusted_builtin` writes it **into the block's text** long before
assembly. By the time a block reaches `assemble`, its own envelope and an
attacker's forged one are the same bytes in the same position. The neutralizer
defused both.

The fix that works splits by *authoring layer*: transcript labels are defused at
`assemble`/`prepare` (which write them), envelope tags at
`frame_untrusted_builtin` / `mcp::frame_untrusted` (which write them). Each
marker is defused immediately below the function that authors it, where the
distinction between "ours" and "theirs" still exists.

A second entry point surfaced from the same reasoning: the system prompt ends
with `ToolRegistry::docs()`, and an MCP tool's description is supplied by the
advertising **server**. Untrusted bytes were reaching the highest-trust region of
the prompt through a string everyone reads as harness-authored.

## Lesson

**A sanitizer can only distinguish real frame from forged frame at the layer
where the real frame is still being written.** Once harness-authored delimiters
have been flattened into a content string, that string is uniform — every later
pass is guessing.

Three consequences:

1. **Split the sanitizer to match the authoring layers, not the marker list.**
   One function over the union of all markers is the natural first draft and it
   is wrong wherever any of that frame is written into content rather than
   around it. Ask, per marker: *who concatenates this, and is the content
   already inside the string at that point?*
2. **A string being "harness-authored" is a claim about the function, not the
   bytes.** The system prompt is harness-authored except for the tool docs;
   the tool docs are harness-authored except for each description; an MCP
   description is entirely attacker-controlled. Trust attaches to provenance,
   and provenance has to be traced to the leaf, not asserted at the root.
3. **Derive the input alphabet from the output alphabet.** What the model must
   not *emit* is exactly what content must not *introduce*. Deriving both from
   one set of constants — plus a test asserting each marker is claimed by
   exactly one input layer — makes drift a build failure instead of a silent
   reopening.

Sanitize by **insertion**, never deletion (LESSON-474's rule, unchanged): an
inserted `_` at the line start cannot mint a label out of its neighbours.

## Why It Matters

The pre-existing test that caught the single-pass version was asserting the
envelope rides verbatim — a *correctness* test, not a security one. Nothing in
the security reasoning predicted the collision; only running the existing suite
did. A fix pass confident enough to skip the full suite would have shipped a
harness that mangles its own untrusted-content envelope on every tool call,
degrading the injection posture it was written to strengthen — LESSON-476's
shape, again.

## Applies When

Writing any sanitizer over a string that carries both harness-authored framing
and untrusted content; adding a new delimiter, envelope, or label to a prompt;
reviewing a "defuse the markers" diff (check *where* it is applied, not just
what it matches); auditing what actually reaches a system prompt in a codebase
with plugins, MCP servers, or any other third-party-supplied metadata.

## Related

- [[LESSON-474]] — the tokenizer-level twin, and the choke-point-below-the-
  format-branch rule this builds on. That one says *which layer*; this one says
  *how many*.
- [[LESSON-475]] — anchor markers where the renderer writes them. The same
  instinct, applied to the input side.
- [[LESSON-476]] — a fix pass can create the exposure it closes.
- [[LESSON-472]] — the output-side containment whose marker sets the input side
  now derives from.

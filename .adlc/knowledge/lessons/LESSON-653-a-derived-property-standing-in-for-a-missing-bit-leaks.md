---
id: LESSON-653
title: "A derived property standing in for a bit the type does not carry will leak — add the bit"
component: "daemon/egress"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["boundary-touch", "provenance", "classifier", "fail-open", "proxy-field", "req-614", "req-619"]
req: REQ-619
created: 2026-09-05
updated: 2026-09-05
---

## What Happened

REQ-614's `Verdict` carried a `sources` set — every in-root id a command
named — and a `BoundaryTouch` kind, but no field saying *where* the touch
was. Both consumers (the `shell` tool's mapping and, copied faithfully,
REQ-619's fold) used `sources.is_empty()` as the proxy for "the touch was
out of root": empty → the permanent sentinel, non-empty → treat the sources
as the boundary evidence. `cat ~/.ssh/id_rsa README.md` produced
`BoundaryTouch` with `sources = {README.md}`; the proxy read that as an
in-root touch, folded to clean `Sources`, and the key's bytes were cleared
for the wire. Every existing `BoundaryTouch` test used a boundary file that
was also the only path named, so the proxy was never contradicted.

A second instance in the same REQ: `ToolProvenance` had no variant carrying
sources *and* unknown, so the model-invoked path collapsed an expansion with
an opaque preamble and a boundary read to bare `Unknown`, dropping the
boundary id — and `/shell allow` would have released it.

## Lesson

When a downstream decision needs a fact the upstream type does not carry,
**add the field** (`out_of_root_touch: bool`, `ToolProvenance::UnknownWith`)
and make every consumer read it. Never infer the fact from a property another
legitimate path can also produce. A proxy fails silently the first time the
world has two things in it; a missing field fails at compile time.

## Why It Matters

Both were fail-open on the charter guarantee, reachable from a `SKILL.md`
that anything with write access to `~/.claude/skills` can plant, with no
prompt at `full`. Neither showed in a suite of thousands of tests, because
a proxy is only wrong on the inputs nobody wrote a test for.

## Applies When

- A classifier or verdict type is consumed by more than one mapping and the
  mapping branches on emptiness, length, or presence of another field.
- Extending a verdict from one consumer to two: copy the consumer's *bits*,
  not its inference.

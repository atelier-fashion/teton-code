---
id: LESSON-633
title: "A detector needs the benign spellings of its own hazard, not just the hazard"
component: "tetond/harness"
domain: "correctness"
stack: ["rust"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["gate", "false-positive", "redirection", "dev-null", "benign-path", "shell", "mutation-testing"]
req: REQ-615
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-615's write gate refuses a `shell` command at a home root when it carries a
top-level `>`. The rule is correct about `echo hi > ~/x`. It was also refusing
`cat missing 2>/dev/null` and `make 2>&1` — a descriptor duplication that
creates nothing, and a write to the null device that creates nothing anywhere.

`2>/dev/null` is the most common redirection in a read-only command, and it is
how the shipped ADLC skill preambles are written. The gate would have made a
home-rooted session unable to *read*, which is the state a user is in **before**
they run `/cd` — so it is the state the feature exists to serve.

The benign-path test had eight rows and none of them was a redirection. Every
one of the gate's positive cases passed, and its own doc comment argued the
false positive away in a parenthesis.

## Lesson

A detector's benign path must include **the benign spellings of the hazard
itself**, not merely unrelated commands that happen not to trip it. "Does the
gate leave `ls -la` alone" is a much weaker question than "does it leave
`ls -la > /dev/null` alone", and only the second finds this class. When writing
the exemption, ask which spellings of the guarded token appear most often in
*correct* use and put those in the table before the adversarial ones.

## Why It Matters

A gate with a false positive on a common benign spelling is not a strict gate —
it is a gate that gets removed. The user hits it on their second command, and
the fix under time pressure is to widen it far past where it needed to be. The
cost of finding this in review is one table row; the cost of finding it in use
is the whole rule.

## Applies When

Writing or reviewing any predicate that refuses, blocks, taints or flags based
on a token, verb, or pattern — especially where the same token has an
established idiomatic use that is not the hazard.

---
id: LESSON-484
title: "Enforce a rule where the decision is made, not where it was convenient to write"
component: "tetond/router"
domain: "architecture"
stack: ["rust"]
concerns: ["correctness", "security"]
tags: ["invariant-placement", "bypass", "classifier-drift", "defense-in-depth", "mutation-testing"]
req: REQ-557
---

## What Happened

REQ-557 introduced one rule — *a remote provider must declare the model it
calls* — and enforced it in four different places, none of which was the place
the decision is actually made. Every one was bypassable (BUG-155):

- The **registration** rule lived in `teton provider add`, so the `config/set`
  RPC bypassed it. Any non-`teton` ACP client could register a modelless
  provider, and the daemon put that provider's id on the wire as its model.
- The **usability screen** lived in `build_router`'s provider-map construction,
  so the two paths that read a provider id straight from config —
  `resolve_freeform`'s `default_provider` and `fallback_for`'s `fallback_id` —
  never saw it.
- The **"declares a model" predicate** was written out at three call sites. One
  trimmed whitespace and two did not, so a provider with `model = " "` was
  simultaneously reported unusable at startup and serving live traffic.
- The **deletion** of a fallback identifier was verified by mutating that
  fallback's *call site*, which said nothing about the byte-identical fallback
  one layer below it — the line that actually reached the network.

## Lesson

Place an invariant at the narrowest point every path must cross, and give it
exactly one definition.

Two questions catch this before it ships:

1. **"Who else can reach this state?"** A rule in a CLI is a rule about that CLI,
   not about the system. If a protocol surface, an RPC, or a config file can
   produce the same state, the rule is in the wrong layer. Enumerate the writers,
   not just the one in front of you.
2. **"Is this predicate written down more than once?"** Two copies of a
   condition are two answers to one question, and nothing observes them
   disagreeing. Put it on the entity and have callers ask.

A corollary for *loading* versus *acting*: it is legitimate for these to differ,
but the difference must be deliberate and stated. BUG-155's fix keeps config
**loading** permissive — a pre-REQ config has to boot far enough to migrate —
while making **registration** fail closed, because a new registration has no
legacy to honour. Same rule, two postures, one written reason.

## Why It Matters

Every bypass here produced a user-visible defect, not a theoretical one: a
fabricated model string sent to real vendor APIs, a paid call billed at $0 and
credited as savings, and a provider the daemon told the user could not serve
turns serving them anyway. The CLI check gave real confidence — it had a passing
end-to-end test — while the actual surface everything else used had none.

It also defeats mutation testing in a way that looks like success. A mutation
against the guard you wrote turns red, so the guard is "verified", while the
unguarded path beside it stays green and untested.

## Applies When

- Adding a required field, permission, or validation to an entity that more than
  one surface can create or modify (CLI + RPC + config file is the classic trio).
- A predicate about domain state ("is this usable / valid / expired / declared")
  is about to be written inline at a second call site — put it on the type
  instead.
- Two guards protect the same outcome (defense in depth). Verify each in
  isolation: if guard A's mutation is caught only by guard B and vice versa,
  neither is actually covered. Test the inner one at its own layer.
- A rule must behave differently on read than on write. That is fine, but write
  down which is permissive and why, or the next reader will "fix" the asymmetry.

Related: [[LESSON-483]] (a mutation check on the outer guard says nothing about
the inner one — the same shape, found one layer up in the same REQ),
[[LESSON-456]] (a state classified by one component and acted on by another),
[[LESSON-441]] (a deletion is verified only by proving restoration breaks
something).

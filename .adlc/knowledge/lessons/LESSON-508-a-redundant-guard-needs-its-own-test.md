---
id: LESSON-508
title: "A redundant guard needs its own test precisely because it is redundant"
component: "daemon/session"
domain: "privacy"
stack: ["rust"]
concerns: ["security", "testing"]
tags: ["mutation-check", "defense-in-depth", "unreachable-invariant", "seam-testing", "LESSON-502"]
req: REQ-570
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

REQ-570 restored the monitor consent path REQ-569 had deleted, under three
independent conditions: a verified presence attestation, a structural rule that
the approver is never the requester (BR-5), and BR-10(a) closing the ungated
`session/create` the original attack opened with.

AC-11's mutation check deleted the BR-5 requester-exclusion — one boolean in a
routing predicate — and **the entire suite stayed green**. Two end-to-end tests
covered that exact attack, including a rewritten REQ-569 regression test, and
neither noticed.

The guard was real, correct, and completely untested.

## Why It Survived

Over the socket the invariant is currently **unreachable**. A connection
declaring `monitor` is parked inside its own handshake while the consent runs,
so it has no reader loop with which to answer itself. Every end-to-end test
therefore passes whether or not the rule exists — none of them can distinguish a
daemon that *enforces* the rule from one that merely never gets the chance to
break it.

That unreachability is a property of an unrelated design choice (where the
monitor gate sits relative to the handshake), held by a different module, and
nothing records that BR-5 depends on it.

## The Lesson

**When you keep a check as defense in depth, test it at its own seam — the very
redundancy that justifies keeping it is what stops every other test from
noticing its absence.**

A check that is load-bearing gets tested for free: remove it and something
breaks. A check that is *redundant* is exactly the one whose deletion is silent,
because the other mechanisms carry the behaviour. So the usual signal — "a test
went red" — is structurally unavailable for the guards most likely to be quietly
dropped in a refactor.

This is LESSON-502 ("an invariant enforced at several seams needs an adversarial
test at each seam") arriving from the other direction. LESSON-502 is about
coverage across seams that all *do* something. This is about a seam that
currently does nothing observable and still must be pinned, because "currently"
is a fact about today's call graph, not a property of the rule.

Two practical rules:

1. **If a mutation of a security check survives, that is a finding about the
   suite, not a licence to delete the check.** The right response is a test at
   the predicate, not a shrug at the redundancy.
2. **Write the reason down at the test.** The next reader needs to know the
   test exists because the socket path cannot reach the case — otherwise it
   looks like a trivial assertion about a boolean and gets deleted as noise.

## How to Apply

- Run mutation checks against *every* guard in a security change, including the
  ones you believe are belt-and-braces. Budget for the belief being wrong.
- When a mutation survives, add a **pure-predicate** test. Pure policy exists to
  be table-tested without a socket (the project's "policy is pure, mechanism is
  gated" pattern) — that is precisely the seam an unreachable-over-the-wire
  invariant is testable at.
- Treat "no end-to-end test can reach this" as a reason the unit test is
  **required**, never as evidence it is unnecessary.

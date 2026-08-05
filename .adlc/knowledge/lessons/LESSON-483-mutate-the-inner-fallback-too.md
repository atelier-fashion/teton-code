---
id: LESSON-483
title: "A mutation check on the outer guard says nothing about the inner one"
component: "daemon/router"
domain: "testing"
stack: ["rust"]
concerns: ["correctness", "test-coverage"]
tags: ["mutation-testing", "fallback-chain", "deletion-verification", "error-classification"]
req: REQ-557
created: 2026-08-05
updated: 2026-08-05
---

## What Happened

REQ-557 deleted a two-link fallback chain: `default_provider` fell back to
`local_provider`, which fell back to the literal id `"local"` (BUG-146 root
cause #1). TASK-047 required a mutation check proving the deletion, and — because
the architecture had already flagged the shape — required **both halves pinned
separately**.

Restoring the outer link (the positional `.find(is_remote)`) turned two tests
red, as expected. Restoring **only** the inner link left the suite **completely
green**.

The reason is the interesting part. The suite's assertions about this condition
ran through the error message, and `unserved_turn_error` classifies from the
**config** — which still said, correctly, that no `default_provider` was set. So
the daemon kept producing exactly the right sentence while the router handed out
a synthesized provider id. The symptom BUG-146 was actually about — a
`route_decided` announcing a provider registered nowhere — was not asserted
anywhere, because every test had reached for the error message instead.

Two tests closed it: a `Router::default_provider()` accessor making the absence
assertable at the type level, and an e2e asserting no `route_decided` ever names
an unregistered provider.

## Lesson

When deleting a fallback **chain**, mutate each link separately, and check that
the test which catches one actually fails for the other. If it does not, the
uncaught link needs an assertion on a *different observable* — not a second test
of the same one.

The generalisation: a mutation check verifies the assertion you already have, not
the deletion you intended. When a mutation comes back green, the finding is the
missing observable — ask *which surface would show this*, and note that a
condition classified from one source (here: config) cannot witness a defect that
lives in another (here: the router's own state).

## Why It Matters

The inner link is the one that ships. It is reached only when the outer one is
absent — the unconfigured, first-run, nothing-set-up path, which is exactly the
state a fallback-identifier defect hurts most and exactly the state least
exercised by fixture-heavy suites. A green mutation check reads as "verified" in
a PR and in a wrapup; here it would have signed off on a live BUG-146 regression
path while the release notes claimed the fallback was gone.

Cost of catching it: one extra mutation run, about five minutes. Cost of missing
it: the original bug, re-shipped under a claim that it was fixed.

## Applies When

- A change removes a fallback, default, or coalescing chain with more than one link
  (`a.or_else(b).or_else(c)`, nested `unwrap_or`, layered config precedence).
- A mutation check comes back **green** — treat that as a finding about the tests,
  never as confirmation that the code is fine.
- The condition under test is classified in one place (a validator, a config
  reader) but *acted on* in another (a router, a dispatcher). Assertions on the
  classifier cannot see defects in the actor.

Related: [[LESSON-441]] (a deletion is verified only by proving restoration breaks
something), [[LESSON-479]] (a one-directional guard), [[LESSON-456]] (a fallback
identifier is not "none").

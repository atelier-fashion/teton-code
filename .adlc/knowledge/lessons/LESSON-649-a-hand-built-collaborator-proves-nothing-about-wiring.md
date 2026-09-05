---
id: LESSON-649
title: "A test that builds the collaborator by hand proves nothing about whether the daemon ever builds it"
component: "daemon/privacy"
domain: "testing"
stack: ["rust", "daemon"]
concerns: ["privacy", "test-coverage"]
tags: ["wiring", "sink", "session-pinned", "e2e", "source-scan", "bug-214", "req-614"]
req: REQ-614
created: 2026-09-05
updated: 2026-09-05
---

## What Happened

REQ-614 shipped `TaintingPrivacySink`, the one sink that reads a block's path
into a `TaintCause` and publishes `session_pinned`, with a green unit test
that constructed the sink by hand, fed it a block, and asserted the event
order. The daemon's prompt turn never built that sink: `run_one_attempt`
handed `Egress::new` the bare `EventBus`, so every turn-path pin was recorded
as permanent `boundary_hit` by a path-less backstop arm and no
`session_pinned` was ever published (BUG-214). The REQ's task file listed
end-to-end tests for exactly this claim as complete; they did not exist.

## Lesson

A claim about *what the daemon does* needs a test that drives the daemon —
the real binary over the socket, asserting on what a client received — or a
source scan that names the construction site. A unit test that instantiates
the collaborator itself can only prove the collaborator works, never that
anyone calls it. When a REQ adds a collaborator that must be installed at N
sites, pin the sites: a scan over the non-test source asserting each
`Egress::new` takes the sink, with a vacuity floor, is cheap and cannot be
satisfied by a hand-built fixture.

## Why It Matters

The 2026-09-04 session that motivated REQ-614 was "pinned for 65 turns and
nobody the wiser"; the REQ's remedy for that silence was the announcement —
and the announcement was never wired on the path that produced the session.
The fix looked shipped for a day. One e2e test (a typed user skill, no shell
call) would have failed on the first run.

## Applies When

- A REQ introduces an event, sink, gate or hook that must be *installed* at
  one or more call sites, not merely defined.
- A task file's verification table names tests: check that every symbol it
  names exists in the tree before marking the task complete.
- Writing the "benign path" for a pin/block claim: the control must run
  through the same daemon path as the claim, or it proves the wrong thing.

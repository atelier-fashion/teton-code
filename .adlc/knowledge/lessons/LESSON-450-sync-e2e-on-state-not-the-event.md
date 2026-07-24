---
id: LESSON-450
title: "An event published before the state applies is not a sync point — wait on a state-derived surface"
component: "daemon/e2e"
domain: "testing"
stack: ["rust", "tokio", "json-rpc"]
concerns: ["reliability", "testing"]
tags: ["event-ordering", "publish-then-apply", "replay", "flake", "attach-retry", "lifecycle-events"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

Writing the cross-process AC-2 chain test (PR #6), the natural synchronization
was "prompt as soon as the live `ready` lifecycle event arrives". But the
daemon publishes `ready` *inside* the consent gate, and the runtime applies
the outcome — flipping the `local_available`/`local_gated` atomics — a beat
**after** the gate returns. A client acting on the live event can race the
flip; worse, a client *attaching* in that gap is truthfully replayed "still
loading and benchmarking" and will never receive another event on its own
connection, because the live `ready` was broadcast before it attached. A
single fixed-window wait here is a latent flake that only fires under
scheduler pressure — exactly the class this suite has had to de-flake before.

## Lesson

When a system publishes an event and *then* applies the state it announces
("publish-then-apply"), the event is an announcement, not a synchronization
point: "event seen" does not imply "state queryable". An e2e test must wait on
a **state-derived surface** instead — here, a fresh client's replayed
lifecycle, which the server derives per attach from `local_tier_available()`
AND the engine slot's own fact, so a replayed `ready` *is* the flip. And
because a client can attach inside the gap and truthfully never hear more,
the wait must **retry the attach** (`connect_when_tier_open`), not extend the
listen window on a connection whose replay already answered.

## Why It Matters

Ordering races between an event bus and the state it describes are
microsecond-wide, invisible on a fast dev machine, and surface as
unreproducible CI flakes weeks later. A test synchronized on the state
surface is immune to the width of the gap; a test synchronized on the event
is betting on it. The same reasoning locates the honest fix when the gap is
user-visible: either the publish moves after the apply, or every consumer
gets a state surface to poll.

## Applies When

- Writing e2e/integration tests that act on a broadcast event whose state
  change is applied by the *caller* of the publisher (gate → runtime,
  service → cache, saga step → aggregate).
- A test needs "the system is now in state X" and the only signal considered
  so far is "the event announcing X arrived".
- Debugging a flake where a late-attaching client waits forever for an event
  that was broadcast just before it connected — replay/snapshot surfaces plus
  reconnect-retry are the fix, not longer timeouts.

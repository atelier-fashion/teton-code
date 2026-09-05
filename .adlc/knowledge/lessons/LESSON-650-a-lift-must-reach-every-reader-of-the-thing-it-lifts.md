---
id: LESSON-650
title: "A lift composed into one predicate still has to reach every reader of the fact it lifts — including the choke point"
component: "daemon/privacy"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["privacy", "routing"]
tags: ["shell-allow", "lift", "egress", "route-pin", "escalation", "bug-215", "req-614"]
req: REQ-614
created: 2026-09-05
updated: 2026-09-05
---

## What Happened

REQ-614's ADR-614-4 composed the `/shell allow` lift into one predicate,
`RoutePin::pins`, and made the seven *route* sites read it — a good sweep.
The egress choke point reads the same fact from a different direction: it
inspects the context's provenance, the unknown-provenance `shell` result was
still a carried block, and nothing on that path consulted the lift. So the
prompt after a lift was routed remote, blocked at egress against
`<unknown-provenance>`, and rerouted local — every turn, for the life of the
session. BR-6 even specified that outcome while BR-4 promised the remedy
(BUG-215). The spec's AC-3, "the next prompt routes remotely", was true of
the `route_decided` line and false of the request.

A second trap inside the fix: making `SessionTaint::mark` escalate a
liftable pin to permanent (so a boundary read after a lift re-pins) turned
every liftable pin permanent one frame after the sink recorded it, because
the turn loop's backstop arm marks `BoundaryHit` for every boundary block —
it holds a path-less `BlockDetail` and cannot know the class.

## Lesson

When a user action *lifts* a consequence, enumerate every reader of the
underlying fact, not just the sites that share the predicate's name. "Where
is the session forced local" and "where is the session's content refused"
are two readers of one fact; a lift that reaches one and not the other
produces a state the spec never described — routed remote, served local.
Write the AC in terms of the observable (the request left the machine, the
mock saw it), never the intermediate (the route said remote).

And an *escalation* (liftable → permanent) is only for writers whose cause
came off the block's path; give it its own method and keep the path-less
backstop on first-cause-wins.

## Why It Matters

The lift is the user's whole remedy for a proportionate pin. A lift that
does not restore remote routing is a lie the client prints (`/shell allow
lifts it if you know the command touched no protected file`) and a session
paying a route decision, a block and a reroute on every turn.

## Applies When

- Adding an override, lift, allow-list or consent that reverses a
  fail-closed decision: list every choke point that makes the decision, and
  test the observable past *each* of them.
- Writing an AC for a routing change: assert on the bytes that reached the
  provider, not on `route_decided`.
- Adding a "first wins / upgrade" rule to a cause register: check which
  writers can see the cause's evidence and which are guessing.

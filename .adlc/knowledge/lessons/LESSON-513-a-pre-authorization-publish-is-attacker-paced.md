---
id: LESSON-513
title: "A pre-authorization publish is attacker-paced — bound the id and budget the notice"
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "reliability"]
tags: ["event-bus", "rate-limiting", "pre-auth", "rejection-notice", "dos", "may_announce_grant"]
req: REQ-572
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-572's setup handlers published a `WebSetupRejected` event on the
**pre-authorization** path — before the `may_drive` gate answered. The
`session_id` in the params was attacker-chosen and bounded only by the frame
cap (~4 MiB), and `EventBus::publish` clones every envelope to every
subscriber over a 256-slot channel that **evicts** a full subscriber, after
which the client never resubscribes. So a same-UID connection — including a
daemon-spawned `Descendant` the ancestry gate bars from all session access —
could spray ~256 refused previews and permanently blind every connected
client to `permission_request`, `privacy_block`, and the rejection notice
itself. The verify pass rated it Critical.

It was the repo's **second** occurrence of the shape: `session_grant_minted`
was flooded the same way and fixed with `ConnState::may_announce_grant`
(server.rs ~695, REQ-569 era). The new surface applied neither the length
bound nor the budget.

## Lesson

An event published before authorization is output the adversary paces. Two
controls, both, every time:

1. **Bound attacker-chosen identifiers first** — the same
   `sessions::within_minted_length` check the post-auth path uses, before any
   publish, clone, or runtime call.
2. **Budget the notice per connection** — an atomic claim
   (`AtomicBool::swap`, the `may_announce_grant` precedent) so a refused
   caller can make the announcement fire once, not at will. A refusal past
   the budget still refuses; only the notice stops.

And for **read-only** endpoints, prefer silence over announcement: a notice
any same-UID peer can raise on demand is one users learn to read past, which
costs the *mutating* path's rejection the attention it exists for.

## Why It Matters

Session events carry the safety-critical surface — consent prompts, privacy
blocks, taint notices. Queue eviction is not lost telemetry; it is the user
no longer being asked. And the recurrence proves the class is architectural:
fixing one publish site does not secure the bus. Every new
attacker-reachable publish needs the bound + budget pair as a design rule,
not a review catch.

## Applies When

Adding any event publish reachable before an authorization gate answers;
handling RPC params carrying caller-chosen identifiers; designing rejection
or audit notices a peer can trigger; reviewing any `EventBus::publish` whose
call site sits above a `may_drive`/grant check. Related: [[LESSON-505]] (an
audit control is judged in the adversarial case), [[LESSON-504]] (a gate's
precondition is part of its claim).

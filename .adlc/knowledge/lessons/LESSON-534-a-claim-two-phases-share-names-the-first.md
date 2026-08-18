---
id: LESSON-534
title: "A claim two phases share names the first phase — read it with the state that tells them apart, and turn a transient refusal into a wait at the classifier"
component: "tetond/runtime"
domain: "routing"
stack: ["rust", "tokio", "daemon", "json-rpc"]
concerns: ["developer-experience", "routing", "lifetime"]
tags: ["held-turn", "turn-queued", "tier-warming", "precedence", "install-claim", "watch", "client-presence", "ghost-turn"]
req: REQ-580
created: 2026-08-17
updated: 2026-08-17
---

## What Happened

Turning BUG-152's `TIER_WARMING` refusal ("retry in a moment") into a daemon-side
wait looked like one `await` — and mostly was. Two things were not obvious from
the request:

1. **The load phase reused the download's claim.** `activate_engine` takes the
   same M-2 install claim `run_install` does, and both `unserved_turn_error` and
   `startup_lifecycle` checked `install_in_flight` *ahead of* the verify state.
   So a running load classified as "download/install is running now". Nobody
   had noticed because the sentence was only ever read by a human mid-wait; the
   moment a *typed* value depended on it (`waiting_on: installing | loading`),
   the held turn would have said "finishes installing" for weights verified
   minutes earlier. Reading the claim *with* `consent_required()` fixed all
   three readers in one precedence (REQ-580 ADR-4), pinned by driving the real
   gate with a parked loader and asserting the hold, the refusal and the replay
   together.
2. **A held turn is not an in-flight turn.** REQ-565 deliberately stopped
   aborting turns on disconnect to protect paid-for work and its cost row. A
   held turn has neither, and letting it run when the tier opened would have
   put a ghost on the engine ahead of the impatient user's next session — the
   exact Ctrl-C-then-restart flow the feature is for. So the hold got a
   `ClientPresence`, withdrawn by the server before REQ-565's drain; a turn
   past the hold never consults it.

Everything else fell out of one rule: the hold reads the *same* typed
classification the refusal renders, sits before anything the turn would spend,
and wakes on a `watch` bumped by every gate transition (re-reading rather than
trusting the wake — a `Superseded` outcome wakes and re-waits).

## Lesson

- When a shared claim or flag is taken by more than one phase, every reader
  must read it together with the state that distinguishes the phases, or every
  reader calls the second phase by the first one's name. Audit precedence the
  moment a *typed* consumer is added to a sentence that was only ever prose.
- A refusal whose own code says "this ends by itself" is a wait waiting to be
  written — but at the classifier that coded it, not as a client retry
  heuristic; before the request builds anything; woken by the state
  transition, re-read on wake; and ended early when the requester leaves,
  because un-started work owes its client nothing.

## Why It Matters

The precedence bug was invisible for four releases and would have shipped a
wrong word into the very notice REQ-580 exists to show. The ghost-turn risk
would have made the feature *worse* than the refusal for the user most likely
to hit it. Both were found by asking "what does the classifier actually read"
and "what does REQ-565's rule assume about the turn" rather than by testing
the happy path.

## Applies When

- Adding a typed event, code, or enum on top of an existing state read that
  previously produced only a sentence.
- Turning any transient refusal (`TIER_WARMING`, `SESSION_BUSY`-class) into a
  wait; deciding what a disconnect owes to work that has not started.
- Any M-2-style claim, guard, or lock reused across phases of one flow.

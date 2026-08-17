---
id: REQ-580
title: "Hold a turn for a warming local tier instead of refusing it: the daemon waits, the session says so"
status: in-progress
deployable: true
created: 2026-08-17
updated: 2026-08-17
component: "daemon"
domain: "routing"
stack: ["rust", "daemon", "cli", "json-rpc", "tokio"]
concerns: ["developer-experience", "routing", "lifetime"]
tags: ["local-tier", "model-loading", "turn-queued", "tier-warming", "startup", "held-turn", "session-busy"]
---

## Description

Start `teton` on a machine with installed weights and type a prompt before the
local model has finished loading. Today the daemon refuses the turn:

```
› hi
>> model still loading — Classification was bypassed for this turn … The 'edit'
   category resolves to 'local', which is unavailable, and no fallback provider is
   configured for it, so 'edit' cannot be routed. qwen3-coder-30b-a3b's weights are
   installed and verified; the daemon is loading and benchmarking them now — the
   local tier opens when that completes. Retry in a moment. No remote provider is
   configured either — `teton provider add` registers one …
>> benchmark qwen3-coder-30b-a3b: first token 275 ms, 80.5 tok/s
>> local model qwen3-coder-30b-a3b ready
›
```

BUG-152 made that refusal honest — coded `TIER_WARMING`, rendered as a notice
rather than an error, and saying plainly that waiting is the whole remedy. It
still leaves the user to do the waiting *and the retrying*: the prompt they
typed is gone, and the paragraph that replaced it is mostly about the routing
decision that could not be made. The daemon knows exactly what it is waiting
for — it published the same fact on the lifecycle stream a moment earlier — and
it knows the moment the wait ends. It should hold the turn and run it then.

The user experience this is for:

```
› hi
>> message queued until qwen3-coder-30b-a3b finishes loading — it will run as
   soon as the local tier opens.
>> benchmark qwen3-coder-30b-a3b: first token 275 ms, 80.5 tok/s
>> local model qwen3-coder-30b-a3b ready
Hello! How can I help you today?
›
```

**Scope.** Exactly the turns that would otherwise be refused with
`TIER_WARMING` — the two states BUG-152 named as ending on their own: an accepted
install in flight, and verified weights mid-load. Nothing else changes: a
settled absence (declined, below the floor, a failed load, an unanswered
proposal) still refuses immediately with the sentence that names its remedy,
and a turn the router sends to a remote provider that can serve it is not held
whatever the local tier is doing (REQ-547 D-3 — the gate withholds the tier,
never the session).

**Provenance.** BUG-152 classified the transient states; REQ-556 filled the
silent load window in an *idle* session with an indicator; REQ-565 settled what
a disconnecting client's in-flight turn is owed. This REQ composes the three:
the classification decides what to hold, the hold fills the same window for a
session that has already spoken, and REQ-565's drain rules are honoured for a
turn that started and refined for one that had not.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| LocalTierState (daemon-internal) | variant | `BelowFloor` \| `Declined` \| `Installing` \| `AwaitingDecision` \| `LoadFailed` \| `Loading` \| `NoEngine` | one reading, one precedence (BUG-152's); `Installing` and `Loading` are the only transient states |
| ClientPresence (daemon-internal) | connected | `watch::Receiver<bool>` or unwatched | read only while a turn is held |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `turn_queued` | a `session/prompt` whose route has nowhere to run **only** because the local tier is `Installing` or `Loading` | `turn_id`, `model_id` (catalog name, never a path), `waiting_on: installing \| loading` — session-scoped; emitted once per held turn, at the moment the hold begins |

There is deliberately no paired "released" event: the turn's own progress
(`route_decided`, the streamed reply, or its refusal) is what follows.

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| hold a turn | the daemon, for any session's own `session/prompt` |

## Business Rules

- [ ] BR-1: **Held only for a wait that ends by itself.** A turn is held when, and only when, its resolved route has nowhere to run because the local tier is not serving *and* the tier's state is one of the two transient ones (`Installing`, `Loading`). Every settled absence refuses immediately, exactly as before. *(informed by BUG-152, LESSON-456)*
- [ ] BR-2: **The hold is announced, once, typed.** The daemon publishes `turn_queued` — session-scoped, naming the turn, the model, and which transient state — before it waits, and never again for the same turn. A client branches on the value, never on prose.
- [ ] BR-3: **The hold ends the instant the tier settles — opened or not.** A tier that opens releases the turn to run exactly as if it had just been sent (route resolved afresh, classifier run for real, head built from that route). A tier that settles *without* opening (the load fails, the model is declined) releases the turn to the settled refusal a turn arriving after the failure would receive. No timeout is invented; the transition is the signal.
- [ ] BR-4: **A held turn has spent nothing.** No classifier call, no tools, no system head, no conversation begun, no title claim, no cost row. The one thing it holds is the session's turn claim (REQ-567 BR-5): a second prompt on the same session while one is held is `SESSION_BUSY`, naming the held turn.
- [ ] BR-5: **The session says so, as a notice.** The interactive surface renders `turn_queued` as a `Notice` — never verbose-gated, never an error — that says the message is queued, names the model, says whether it is finishing installing or loading, and that the turn runs by itself when the tier opens. No countdown, no ETA (REQ-556 BR-5's rule).
- [ ] BR-6: **A client that leaves mid-hold gets no ghost.** If the issuing connection disconnects while its turn is still held, the turn ends at once with the refusal it would have carried without the hold, and nothing runs when the tier later opens. A turn that has *started* keeps REQ-565's drain semantics unchanged. *(informed by REQ-565)*
- [ ] BR-7: **One classifier.** The decision to hold, the sentence a refused turn carries, and the lifecycle replay's description of the tier read the same state in the same precedence. In particular the *load* phase — which takes the same M-2 claim the download does — reads as `Loading` in all three, not as a download that is not happening.
- [ ] BR-8: **Additive on the wire.** No method changes; one new event. An older client that does not know `turn_queued` ignores it and simply sees the reply arrive; an older daemon keeps refusing with `TIER_WARMING`, which the client still renders.

## Acceptance Criteria

- [ ] AC-1: On the real daemon, a prompt sent while verified weights are loading (a) is answered by a `turn_queued` event scoped to its session, typed `loading`, naming the model — observed *before* any `ready` on that connection; (b) stays pending while the load runs; and (c) completes `end_turn` after `ready`, served by the engine the loader committed, with the `turn_id` the announcement named — with no second `session/prompt` from the client. *(e2e over the socket, `TETON_FAKE_ENGINE_LOADER` + the new `TETON_FAKE_ENGINE_LOADER_DELAY_MS` seam)*
- [ ] AC-2: A held turn whose load then fails is refused with `UNKNOWN_PROVIDER` and the load's own reason, without "retry" advice, the instant the failure is applied. *(unit)*
- [ ] AC-3: A turn against a settled absence (a recorded load failure) is refused immediately with `UNKNOWN_PROVIDER` and publishes no `turn_queued`. *(unit)*
- [ ] AC-4: A turn whose route resolves to a servable remote provider is attempted there while the local tier is loading, and publishes no `turn_queued`. *(unit)*
- [ ] AC-5: With the issuing client's presence withdrawn during the hold, the turn ends with `TIER_WARMING` and the engine records zero calls after the tier opens. A presence already withdrawn before the hold begins ends it without waiting. *(unit; the server drops the connection's liveness before REQ-565's drain)*
- [ ] AC-6: The interactive surface renders `turn_queued` as a `Notice` line beginning `message queued`, naming the model and `finishes loading` / `finishes installing` by the typed value; it renders no error line and no ETA. *(unit over the render seam)*
- [ ] AC-7: A load in flight (the loader's claim held) reads as `Loading` in the hold predicate, in `unserved_turn_error`'s sentence, and in the lifecycle replay alike — none say "download/install is running". *(unit, through the real gate with a parked loader)*
- [ ] AC-8: `turn_queued` round-trips on the wire under that name with a flat payload of exactly `turn_id`, `model_id`, `waiting_on` (+ envelope `seq`, `session_id`). *(protocol unit)*
- [ ] AC-9: Every consent outcome applied to the runtime wakes a held turn, which re-reads the state: a no-op outcome (`Superseded`) leaves it waiting; `Ready` ends it. *(unit)*
- [ ] AC-10: The mutation "hold disabled" fails AC-1's unit twin, AC-2 and AC-5 — observed, not reasoned. *(recorded below)*

## External Dependencies

- None new. `tokio::sync::watch` (already a dependency; the event fence uses it).

## Assumptions

- The two transient states always end through `apply_consent_outcome` — the startup flow and `model/set` both funnel there. A loader that reports `Ready` without committing an engine (LESSON-443's shape) is already a wedge (`local_available` stays false); under this REQ a held turn in that state waits until its client leaves rather than being refused each time. Accepted: it is a broken loader either way, and the client's own Ctrl-C ends the hold.
- The interactive surface does not animate during a held turn (the REQ-556 indicator lives in the entry frame, which is down during a turn); the queued notice and the lifecycle's own `benchmark`/`ready` lines are what move. A spinner inside the turn pump is a possible follow-up, not part of this REQ.
- A second client attached to the same session watches nothing further when the *issuing* client leaves mid-hold (BR-6). Accepted over the alternative (a ghost turn on the engine ahead of the user's next session).

## Open Questions

- [ ] OQ-1: Should a held turn survive its client's disconnect *if the daemon knows another client is attached to the session*? Today the answer is no (BR-6); the daemon does not consult the attach table for it. Revisit if a multi-client session workflow needs it.

## Out of Scope

- Holding a turn for a tier that is *awaiting a decision* (an unanswered proposal). Waiting does not answer it; the user does. REQ-547 D-3 stands.
- Queueing *multiple* prompts per session while the tier warms. The interactive CLI is sequential; a second prompt on a held session is `SESSION_BUSY` (BR-4), which is the truthful answer.
- Any change to the `TIER_WARMING` code or sentence. Both remain for the paths the hold does not cover (a fallback that lands on the warming tier after a remote primary fails; an older daemon).
- A wait bound / timeout. The tier's transition is the signal; a bound would be an ETA in disguise.

## How this landed

Implemented directly on branch `claude/teton-message-queue-startup-5583be`
(2026-08-17), from a screenshot-and-sentence request, without the `/proceed`
pipeline. This spec was written alongside the code so the contract is on
record; `architecture.md` beside it names the four decisions the code
comments cite as ADR-1..ADR-4. Flip `status` to `complete` at merge.

**Mutation record (AC-10).** With `hold_for` forced to `None`, the daemon unit
suite fails exactly `a_turn_that_meets_a_loading_tier_is_held_and_then_served`,
`a_held_turn_is_refused_with_the_settled_reason_when_the_load_fails` and
`a_held_turn_ends_without_running_when_its_client_disconnects` (3 of 10 in
`held_turns`). With `local_tier_state`'s precedence reverted to read the install
claim ahead of the verify state,
`a_load_in_flight_reads_as_loading_in_the_hold_and_the_lifecycle_alike` fails.
Both observed on 2026-08-17.

# REQ-580 — Architecture: Hold a turn for a warming local tier

## Approach

Replace one `break` with one `await`, at the one point on the turn path where
that is honest. The daemon already classifies why a turn cannot be served
(`unserved_turn_error`, BUG-152) and already learns the moment the local
tier's gate changes (`apply_consent_outcome`). This REQ connects the two: a
turn whose route has nowhere to run *because* the tier is in one of the two
transient states waits on the gate, then routes itself afresh. Everything
else — the sentences, the codes, the lifecycle replay, the router — is reused
by reference, and the hold is expressed as one new event and one new parameter.

- **Daemon (`tetond/src/runtime.rs`)** — the classifier's state read is
  extracted into a typed `LocalTierState` (seven variants, one precedence) that
  `unserved_turn_error` renders and the hold reads. `attempt_source` factors
  the "what would this attempt run on?" question out of `run_one_attempt` so
  the hold asks the identical question ahead of the turn. `run_prompt_turn`
  gains the hold between `dispatch_route` and the tools/head/conversation
  build, publishes `turn_queued`, awaits `tier_transitions` (or the client
  leaving), rebuilds the router, re-dispatches, and proceeds. The title spawn
  moves after the hold. `apply_consent_outcome` bumps the watch on every call.
- **Server (`tetond/src/server.rs`)** — each connection owns a
  `watch::channel(true)`; the receiver rides into every turn it spawns as
  `ClientPresence`; the sender is dropped at teardown *before* REQ-565's drain.
- **Protocol (`teton-protocol/src/events.rs`)** — `Event::TurnQueued`
  (`turn_queued`), `TierWarming { Installing, Loading }`.
- **Client (`teton/src/session_ui.rs`)** — one render arm: a `Notice`.
- **Test seam** — `TETON_FAKE_ENGINE_LOADER_DELAY_MS`, read only inside the
  already-gated fake loader, so the e2e can put a prompt inside the load window.

## Decisions

### ADR-1 — Hold before the turn is built, not at the `NoTierAvailable` arm

The refusal happens deep in the `'turn` loop, after tools, system head and the
carried conversation were built from the pre-hold route. A turn served after
the wait must be built from the route it is served *by* (REQ-567 BR-7's rule,
applied to a single turn), and the classifier — bypassed while the tier was
down — must actually run. So the hold sits between `dispatch_route` and the
build, and re-dispatches. The predicate is `attempt_source` (the attempt's own
reading, factored out rather than duplicated) crossed with `local_tier_hold`
(the classifier's own reading, plus the availability guard — see the doc on
`local_tier_state` for why that guard is load-bearing). `Unservable` is typed
(`LocalTierDown` vs `RemoteWithoutModel`) so a misconfiguration the tier's
arrival would not fix is never held for.

### ADR-2 — A `watch` bumped on every applied consent outcome

Every transition of the tier's gate — startup load, `model/set`, failure,
decline — funnels through `apply_consent_outcome`. It now bumps a
`watch::Sender<u64>` unconditionally (including the two no-op outcomes), and
the waiter re-reads `local_tier_hold` on every wake. Unconditional so there is
no second place that has to know which outcomes matter to a held turn; a
`watch` rather than a `Notify` because subscribe-then-check-then-wait cannot
miss a transition that lands between the check and the wait. No timeout: the
transition is the signal, and a bound would be an ETA in disguise.

### ADR-3 — A held turn ends when its client leaves

REQ-565 deliberately stopped aborting in-flight turns on disconnect, to protect
work already paid for and the cost row that records it. A *held* turn has
neither, and letting it run when the tier opens puts a ghost on the local
engine ahead of whatever session that user opens next (the impatient-Ctrl-C-
then-restart flow is exactly the one this REQ is for). So `run_prompt_turn`
takes a `ClientPresence`; the hold `select!`s on it; the server withdraws it
before the drain. A turn past the hold never consults it. Accepted cost: a
second client attached to the same session sees nothing further if the issuing
client leaves mid-hold.

### ADR-4 — The load phase reads as `Loading`, in every reader

`activate_engine` takes the same M-2 claim the download does, so a reader that
checked `install_in_flight` ahead of the verify state called a running load a
running download — and a held turn would have been announced "until it finishes
installing" for weights verified minutes earlier. `local_tier_state` and
`startup_lifecycle` now both read the claim *with* the verify state:
unverified-and-claimed is `Installing`; verified is `Loading`/`LoadFailed`/
`NoEngine` by the loader facts. Pinned by a test that drives the real gate with
a parked loader and asserts all three readers together (LESSON-456).

## Test economy

| Layer | What it proves |
|---|---|
| `teton-protocol` unit | wire name, flat payload, typed `waiting_on`, key set |
| `tetond` unit (`runtime::tests::held_turns`) | held→served; held→settled refusal; no hold on settled; no hold on servable remote; presence ends the hold, no ghost; every outcome wakes; `attempt_source` table; typed state ↔ code agreement; load phase reads `Loading` in all readers |
| `tetond` e2e (`consent_matrix::req580_…`) | the report's exact shape on the real binary: `turn_queued` before `ready`, reply after, same `turn_id`, served by the committed engine, one announcement |
| `teton` unit (`session_ui`) | the notice: class, wording by typed value, no ETA |
| `teton` e2e | `message queued` joins `assert_no_turn_ran`'s guard list |

---
id: BUG-177
title: "Every client attach replays the model lifecycle into every open session"
status: open
severity: low
created: 2026-08-17
updated: 2026-08-17
component: "daemon/server"
domain: "events"
stack: ["rust", "json-rpc", "daemon", "cli"]
concerns: ["developer-experience", "events"]
tags: ["model-lifecycle", "handshake", "replay", "event-scope", "broadcast", "shell-tool", "noise"]
---

## Description

The startup `model_lifecycle` sequence (`probed` → … → `ready`) is meant to be
replayed **to a client that just attached**, so it learns the tier's state
(REQ-544 BR-9 / AC-8). It is delivered by publishing on the daemon-wide bus:

```rust
// crates/tetond/src/server.rs, handshake — "Published after the subscribe
// above, so this client receives it"
for lifecycle in daemon.runtime.lifecycle_events() {
    daemon.events.publish(None, Event::ModelLifecycle(lifecycle));
}
```

A daemon-scoped publish reaches **every** subscriber, not only the new one. So
every attach — a `teton doctor` or `teton cost` in another terminal, and above
all every `teton …` a session's own `shell` tool spawns — re-prints
`>> probe: 48.0 GiB RAM — clears the local-tier floor` and
`>> local model qwen3-coder-30b-a3b ready` into every session that is open,
in the middle of whatever it was doing.

Observed 2026-08-17 on v0.1.19: a session in which the model ran
`teton provider list` and then `teton policy show` showed the two-line replay
twice, interleaved with the tool status lines. It reads as the tier
re-announcing itself for no reason. (The `a CLI client attached` line beside
it is a *deliberate* daemon-wide announcement — REQ-544's `daemon_client_attach`
— and is not this bug.)

## Reproduction Steps

1. Start a session: `teton`. Wait for `local model … ready`.
2. In a second terminal, run `teton doctor` (or, inside the session, ask the
   model to run any `teton …` command through the `shell` tool).
3. Watch the first session.

## Expected Behavior

The first session prints `a CLI client attached (protocol 2)` (that event is
daemon-wide by design) and nothing else. The lifecycle replay goes to the
attaching client only — it is *that* client's catch-up, not news to anyone
already attached.

## Actual Behavior

The first session also prints the full lifecycle replay (`>> probe …`,
`>> local model … ready`, or the `disabled`/`awaiting_decision` line if the
tier is not up) on every attach, however many times it has already seen it.

## Environment

- Platform: macOS (Apple Silicon), launchd-started daemon
- Version: teton 0.1.19 (protocol 2); present in every version since REQ-544

## Root Cause

**Confirmed at the wire (2026-08-17).** The replay is a **broadcast used as a
unicast**: `do_handshake` (`crates/tetond/src/server.rs`) had no delivery path
to the just-subscribed client other than the bus, and `EventBus::publish`
fans out to every subscriber. Session-scoped events reach only their sessions'
clients through the REQ-568 filter (`should_forward`), but `model_lifecycle` is
daemon-scoped by definition — `should_forward(None, …)` is `true` for every
connection — so nothing narrowed it.

The regression test
`ac_matrix::bug177_a_replayed_lifecycle_reaches_only_the_client_that_attached`
(`crates/tetond/tests/e2e/ac_matrix.rs`) reproduced it before the fix: with A
attached and quiescent, B's attach put B's `probed` (seq 6) and `ready`
(seq 7) on A's stream between B's and C's `daemon_client_attach` markers.
The absence is decided by ordering, not by a timer — the same pattern
`multi_client.rs` uses for every "B did not receive X" claim.

Nothing narrower than a per-connection delivery would do: a bus scope for
"this connection only" is a second routing mechanism for a case the daemon
already has one for (REQ-569 BR-6's routed consent frames).

## Resolution

The replay is now **routed to the attaching connection** instead of published:
`do_handshake` builds each lifecycle stage into an event frame with
`routed_event_frame` (the renamed `consent_event_frame` — the seq still comes
from the bus, so a replayed frame can never wear a broadcast frame's number on
the same connection) and `try_send`s it on the connection's own `out_tx`,
right behind the handshake result. The frames sit on the same FIFO channel as
the result, so the result is on the wire first and the replay precedes anything
the connection is answered next; the REQ-568 fence is not involved because
nothing was delivered to the subscription. No bus publish, so no other
subscriber hears it.

Consequences beyond the noise: a foreign attach can no longer reset another
session's REQ-556 loading indicator (`LoadingIndicator::observe` folds every
lifecycle event it renders); and the consent-suite comment "a client attaching
in the gap … will never see more events on its own connection" is now strictly
true. The CLI is unchanged — the replay still reaches the client that asked for
it, which the new test's positive controls (A, B and C each receive their own
`probed` → `ready`) hold.

## Files Changed

- `crates/tetond/src/server.rs` — `do_handshake`: lifecycle replay goes out on
  the connection's `out_tx` as routed frames, not `events.publish(None, …)`;
  `consent_event_frame` → `routed_event_frame`, doc names the three routed
  deliveries.
- `crates/tetond/src/broadcast.rs` — `EventBus::next_seq` doc names the replay
  as a second routed consumer.
- `crates/tetond/tests/e2e/ac_matrix.rs` — `bug177_…` regression test (fails
  on the unfixed daemon; ordering-decided absence with positive controls).
- `CHANGELOG.md` — `[Unreleased]` → Fixed entry.
- `docs/manual-verification.md` — BUG-177 confirmation runbook (OUTSTANDING
  until the shipped binary is dogfooded).

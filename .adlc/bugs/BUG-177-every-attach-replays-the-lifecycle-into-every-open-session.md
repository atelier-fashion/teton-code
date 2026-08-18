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

(to confirm on fix — the mechanism above is read from the source, not yet
mutation-tested) The replay is a **broadcast used as a unicast**: the only
delivery path a handshake has to the just-subscribed client is the bus, and the
bus has no per-connection scope. Session-scoped events reach only sessions'
own clients through the REQ-568 filter, but `model_lifecycle` is
daemon-scoped by definition, so nothing narrows it.

Fix directions, in order of preference:

- Deliver the replay on the new connection's own outbound (`out_tx`), as
  ordinary event frames with envelope seq numbers, immediately after the
  handshake result — no bus publish. The comment's own reason for the bus
  ("so this client receives it") is satisfied more directly. Mind the
  REQ-568 event fence: the replay must not overtake the handshake result on
  the wire.
- Or a bus scope for "this connection only", if one is wanted for other
  catch-up material later.

Interaction to keep in view: the CLI's REQ-556 `LoadingIndicator::observe`
folds *every* lifecycle event it renders, so today a foreign attach's replayed
`ready` also resets/hides another session's indicator mid-load. Delivering to
the attaching client only removes that side effect too.

## Resolution

(filled after fix)

## Files Changed

- (none yet)

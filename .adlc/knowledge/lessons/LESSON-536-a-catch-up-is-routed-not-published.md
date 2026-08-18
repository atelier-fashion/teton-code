---
id: LESSON-536
title: "A per-connection catch-up is routed, not published — and a broadcast chosen for want of a route outlives the route's arrival"
component: "daemon/server"
domain: "events"
stack: ["rust", "tokio", "json-rpc"]
concerns: ["developer-experience", "events", "testing"]
tags: ["broadcast-vs-unicast", "event-scope", "handshake", "replay", "catch-up", "routed-frames", "ordering-decided-absence", "bus-audit"]
req: BUG-177
created: 2026-08-17
updated: 2026-08-17
---

## What Happened

REQ-544 gave every attaching client a *replay* of the startup `model_lifecycle`
sequence so it would learn the local tier's state. At the time the daemon had
exactly one way to reach a client — the bus — so the handshake did
`events.publish(None, ModelLifecycle(...))` right after subscribing the
newcomer, with a comment giving the reason: "published after the subscribe
above, so this client receives it". True, and incomplete: a daemon-scoped
publish reaches every subscriber, and `model_lifecycle` is daemon-scoped by
definition, so REQ-568's session filter (`should_forward(None, …)` → `true`)
passed it to everyone. Every `teton doctor` in another terminal — and every
`teton …` a session's own shell tool spawned — reprinted `>> probe …` /
`>> local model … ready` into every open session mid-turn, and reset each one's
REQ-556 loading indicator on the way.

REQ-569 later *added* a per-connection delivery path (routed consent frames on
a connection's `out_tx`, seq drawn from the bus). Nobody went back to the
replay: the mechanism chosen for want of an alternative outlived the
alternative's arrival by fifteen REQs, and was found by dogfooding on 0.1.19
(BUG-177), not by any test — the leak read as the tier "re-announcing itself".

The fix routes the replay on the attaching connection's own outbound, right
behind the handshake result. The regression test decides the *absence* by
ordering rather than by a timer: each attach publishes `daemon_client_attach`
to the already-subscribed clients *before* the newcomer is subscribed and
replayed, so on client A's FIFO stream B's marker precedes anything B's
handshake could leak and C's marker follows it — a leaked replay has exactly
one place to land, and an empty gap is a decided fact. Positive controls sit
in the same test (A, B and C each receive their own replay; A hears both
markers), so it cannot pass by the daemon being slow. It failed on the unfixed
daemon with B's `probed`/`ready` at seq 6–7 sitting exactly there.

## Lesson

1. **Ask who the audience is before choosing the mechanism.** A catch-up, a
   replay, a "so this client learns X" is addressed to *one connection*: it is
   a routed delivery, and a bus publish is the wrong primitive for it however
   convenient the subscription just created is. If the bus is the only path,
   the comment should say "TODO: route once a per-connection path exists", not
   justify the broadcast — the justification is what let it stand.
2. **When a routed path arrives, audit the bus.** Grep every
   `publish(None, …)` and every publish that follows a `subscribe` and ask
   whether its intended audience is everyone. REQ-569 added routed frames and
   the audit did not happen; the leak was hiding behind a scoping story that
   was true of session-scoped events and silent about daemon-scoped ones.
3. **Assert an absence by ordering, not by a timer.** Bracket the window with
   two markers the daemon publishes in a known order and read the gap; keep
   the positive controls in the same test. `multi_client.rs` already did this
   for session scoping — reuse it for every "X did not receive Y" claim rather
   than draining a fixed window and hoping.

## Why It Matters

A broadcast used as a unicast is invisible to a single-client test suite —
every automated client got its replay, so every AC passed — and it scales
with the very feature that made it loud: the model's shell tool attaches a
fresh CLI on every `teton …` it runs, so the noise grew with the product's
own dogfooding. Beyond noise, a per-connection message on a daemon-wide bus is
a class of leak: whatever the catch-up carries, everyone hears it. Today that
is hardware and a model name; a future catch-up (a session summary, a pending
consent, a provider list) would have gone the same way with the same comment
above it.

## Applies When

- Adding anything a daemon says to a client *because that client just
  connected or attached* — replays, snapshots, catch-ups, "current state"
  frames.
- Adding a new routed/scoped delivery mechanism to a system that until then
  only had a broadcast one — go back and reclassify the existing publishes.
- Writing a test that a connection did **not** receive something over a FIFO
  stream — bracket with markers, keep the positive controls, no timers.

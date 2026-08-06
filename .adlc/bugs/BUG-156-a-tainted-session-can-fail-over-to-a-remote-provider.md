---
id: BUG-156
title: "A session pinned local by the privacy taint backstop can fail over to a remote provider"
status: fixed-by-REQ-558
severity: high
created: 2026-08-06
updated: 2026-08-06
component: "daemon/router"
domain: "privacy"
stack: ["rust"]
concerns: ["security", "privacy", "correctness"]
tags: ["br1", "session-taint", "failover", "egress", "backstop-bypass"]
req: REQ-558
---

## Description

REQ-544 BR-1 guarantees that content from a `local-only` boundary never reaches a
remote provider. The **session taint backstop** is the second half of that
guarantee: a session whose context has touched boundary content — or carries a
result of *unknown provenance*, such as a `shell` result — is pinned to the local
tier for every subsequent turn, regardless of routing policy.

**That pin can be routed around by the mid-turn failover path.**

When a turn's provider fails with a Fallback-class error, the daemon calls

```rust
let fo = router.on_provider_failure(core_phase, &pid.0, class);   // runtime.rs:1429
```

Note `core_phase` — the **session's** phase, not the pinned route's. The taint pin
sets `phase: None` precisely so it carries no policy identity, but that field is
never consulted here. `Router::fallback_for` then looks up the phase's policy and
returns its fallback whenever the failed provider is that policy's primary.

So for a tainted session whose phase policy names the **local provider as
primary** with a **remote fallback**, a local failure hands the turn — and its
accumulated, taint-marked context — to the remote provider.

## Reproduction

Confirmed by execution against `main` @ `4540387` (a temporary test in
`crates/tetond/src/router.rs`):

```rust
let router = Router::new(
    vec![RoutingPolicy {
        phase: CorePhase::Implement,
        provider_id: "local".to_owned(),          // primary IS the local tier
        fallback_id: Some("frontier-remote".to_owned()),
    }],
    None,
    Some("local".to_owned()),
)
.with_provider("local", "qwen", …, ProviderHealth::Healthy)
.with_provider("frontier-remote", "claude-opus-4", …, ProviderHealth::Healthy);

let pinned = router.resolve_local_pin("tainted: pinned to local (BR-1 backstop)");
assert_eq!(pinned.provider_id.as_ref().unwrap().0, "local");

let outcome = router.on_provider_failure(
    Some(CorePhase::Implement), "local", FailureClass::MalformedResponse);
```

Result:

```
BR-1 HOLE CONFIRMED: a tainted session failed over to 'frontier-remote'
reason: provider `local` fell back after a malformed response
        Continuing on the fallback 'frontier-remote'.
```

The triggering config is not contrived — "try local, fall back to remote if it
struggles" is a sensible thing to write, and is the shape the local-first pitch
invites:

```toml
[[routing]]
phase = "implement"
provider_id = "local"
fallback_id = "deepseek"
```

## How far the evidence goes

**Confirmed by execution**: the router returns a remote failover route for a
tainted, locally-pinned session.

**Confirmed by code reading**: the daemon reaches that call after a failed
attempt, and passes the session's phase rather than the route's.

**Not yet executed end to end**: whether bytes reach the wire depends on the
egress choke point, which still runs. For a session tainted by **boundary-file
content**, egress would likely block the payload — provenance matches a glob.

For a session tainted by **unknown provenance** (a `shell` result), it would not:
that content matches no boundary glob, so the egress check has nothing to catch.
**That case is the reason the taint backstop exists at all** — REQ-544 C-2 calls
it "the backstop for the model-paraphrase residual BR-1 provenance alone cannot
catch." A backstop that a fallback can route around is not a backstop.

Closing that leg is REQ-558 TASK-057's egress-capture work.

## Root Cause

The pin and the failover path disagree about what identifies a route.

`resolve_local_pin` deliberately produces a route with no policy identity
(`phase: None`) — the correct instinct. But `on_provider_failure` does not read
the route's identity; it reads the **session's** phase from the caller. So the
pin's carefully-empty field is bypassed by a parameter that was never part of the
pin.

This is LESSON-456's shape once more: a state classified by one component
(the pin: "this route belongs to no policy") and acted on by another
(the failover: "look up this session's policy"), with nothing observing the
disagreement.

## Fix

Already implemented on `feat/REQ-558-purpose-routing-categories` (TASK-050,
commit `3b83098`), incidentally rather than deliberately: `on_provider_failure`
now takes `&Route` and reads the fallback off **the route's own
`CategoryResolution`**. The taint pin carries `resolution: None` by construction —
it consults no binding — so it has no fallback to hand out, and the failover path
cannot manufacture one from the session's phase.

Pinned by `a_tainted_session_cannot_fail_over_to_a_remote_provider`.

## Why this has its own record

The fix ships inside REQ-558, but the defect is independently reachable on
released versions and has nothing to do with routing categories. A privacy bug
that exists only as a paragraph in a routing REQ's changelog is a privacy bug
nobody can find later.

## Applies When (for the lesson this feeds)

- A guard produces a deliberately-empty identity field to signal "this is not a
  normal decision" — and a downstream consumer reads that identity from somewhere
  else instead.
- A privacy or safety pin is enforced on the primary path, and a *recovery* path
  (retry, fallback, degrade) re-derives its target independently.

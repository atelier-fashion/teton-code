---
id: BUG-186
title: "`NotRunReason` and the wire `DynamicOutcome` are closed, so a future variant drops the whole `skill_invoked` event"
status: resolved
severity: low
created: 2026-08-20
updated: 2026-08-22
component: "protocol"
domain: "clients"
stack: ["rust", "json-rpc"]
concerns: ["extensibility", "reliability"]
tags: ["additivity", "serde", "skew", "skill_invoked", "req-585", "lesson-524"]
---

## Description

`NotRunReason` and the wire `DynamicOutcome` travel **daemon → client only**,
and both are closed enums with no `#[serde(other)]` — the opposite polarity to
`PermissionSubject`, which is correctly tolerant because it must fail closed.

The client's event reader is `serde_json::from_value(params).ok()?`, so a
future fifth `NotRunReason` or a fifth outcome `kind` **silently drops the
entire `skill_invoked` event** on an older client: no echo line, no `/verbose`
outcomes, and BR-12's "every invocation echoes one" quietly becomes false with
nothing said.

REQ-585 gave `SkillSkipped.name` a four-leg additivity test for a far smaller
degradation than this.

## Impact

Forward-compatibility only, and gated in practice by ADR-2's handshake — but
the failure is silent and total for the event, where failing closed buys
nothing: both surfaces are cosmetic.

## Suggested fix

Add `#[serde(other)] Unknown` to `NotRunReason` (rendered as a bare "not run")
and to the wire `DynamicOutcome` (rendered as "outcome unknown to this build"),
with the usual four-leg skew test. Keep `PermissionSubject` closed-with-`other`
as it is — that one's `Unrecognized` arm is load-bearing and must stay a
refusal.

Related, same file: `RefusalReason`'s doc says a client inventing a third door
"fails the params rather than having its answer silently rendered as one of
these two". The real consequence is stronger and unstated — `permission/respond`
fails at deserialization, the waiter is never resolved and never withdrawn, and
`rx.await` has no timeout, so the turn parks until the connection drops. Either
amend the doc or withdraw the waiter when a `permission/respond` for a live
request fails to parse.

## Found

REQ-585 Phase 5 verify (architecture review), 2026-08-20.

## Resolution — 2026-08-22

Closed by adding `#[serde(other)] Unknown` to both enums, rendered as "outcome unknown to this build" and "it did not run". `PermissionSubject` deliberately stays closed. Four-leg skew test, mutation-checked. The related `RefusalReason` note was resolved by amending the doc rather than withdrawing the waiter: the parse is what failed, so the `request_id` is not reliably in hand, and if it were, a malformed message would become a way to cancel any session's standing prompt.

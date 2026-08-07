---
id: BUG-158
title: "A new CLI cannot read a running v0.1.10 daemon's config — policy show and config get fail with a raw serde error"
status: fixed
severity: medium
created: 2026-08-07
component: "protocol"
domain: "upgrade-path"
found_by: REQ-561 Phase-5 verify panel
fixed_by: "#64 — fix(protocol): gate the v1/v2 ConfigSnapshot skew at the handshake"
introduced_by: REQ-558
---

## What happens

After upgrading the CLI but **before** the daemon restarts, `teton policy show`
and `teton config get` fail with a raw serde error rather than a sentence.

ADR-007 makes this pairing normal, not exotic: socket and lock filenames are
deliberately stable "so a newly-installed CLI finds an already-running old
daemon". So the window is every upgrade until the daemon is restarted.

## Why the handshake does not catch it

`PROTOCOL_VERSION_MIN` and `PROTOCOL_VERSION_MAX` are both still `1`. The
handshake therefore **succeeds**, and the incompatibility surfaces later as an
unhandled deserialization failure out of `Connection::call`.

## The actual incompatibility

REQ-558 reshaped `ConfigSnapshot`. Verified against the released tag:

| | v0.1.10 (`68cc62a`) | today (`origin/main`) |
|---|---|---|
| `providers` | `Vec<ProviderConfig>` | same |
| `tiers` | **absent** | `Vec<TierRouteView>` |
| `routing` | `Vec<RoutingRule>` (phase-keyed: `{phase, provider_id, fallback_id?}`) | `Vec<CategoryRouteView>` (category-keyed) |

A v0.1.10 response fails on the missing `tiers` field and on the `routing` row
shape. Both are structural: no field default can bridge them.

## This is not REQ-561's break — and REQ-561's own additive fields are handled

The Phase-5 panel flagged this as a `content_class` regression introduced by
REQ-561. The *mechanism* it found was real (`content_class` had no serde
default), but the *consequence* was misattributed: deserialization already fails
on `tiers` long before `content_class` is reached.

REQ-561 added `#[serde(default)]` to `content_class` and `reached` anyway, since
they are genuinely additive and four sibling fields in the same file already do
this. That is correct hygiene, but it does **not** fix the break above and was
not claimed to.

## Fixed

Resolved on `main` by **#64** (`e523d3d`), independently and while REQ-561 was
still in flight — exactly the remedy suggested below. `PROTOCOL_VERSION_MIN` is
now `2`, so a v1 daemon is turned away at the handshake with a sentence naming
which half is stale and the restart command, instead of a serde error from a
command the user just typed.

#64 also pinned the reasoning with a test carrying a verbatim v0.1.10
`config/get` snapshot and asserting it does **not** deserialize — with the
instruction that if anyone ever makes it parse, `PROTOCOL_VERSION_MIN` must drop
back to 1 in the same change. That keeps the version pin and the fact it rests
on from drifting apart.

REQ-561's `#[serde(default)]` on `content_class` and `reached` remains correct
and is unaffected: it does not make a v1 snapshot parse (that fails on `category`
and `tiers`, which have no defaults), so it neither contradicts #64's test nor
weakens the gate. It covers additive skew *within* v2, which is a different
problem.

## Suggested fix (as filed — now implemented by #64)

Bump `PROTOCOL_VERSION_MIN` so the handshake refuses the skew with a clear
sentence — "your daemon is running an older protocol; restart it with
`teton daemon restart`" — instead of surfacing a serde error from a command the
user just typed.

This was deliberately **not** done inside REQ-561: bumping MIN there would have
had REQ-561 take both the blame and the remedy for another REQ's wire break, and
would change handshake semantics for every command rather than the two that read
this field. It deserves its own change with its own test.

The test to add: a client at the new version against a server advertising the old
one gets a typed, human-readable refusal — not `Error("missing field ...")`.

## Related

- REQ-558 (the `ConfigSnapshot` reshape)
- ADR-007 (stable socket/lock names, which make the skew window routine)
- REQ-561 Phase-5 verify panel, where it was found

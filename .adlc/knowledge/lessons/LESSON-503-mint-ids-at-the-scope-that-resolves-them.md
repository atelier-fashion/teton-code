---
id: LESSON-503
title: "An id must be minted at the scope that resolves it — and tightening isolation surfaces the latent collisions ambient broadcast was hiding"
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "reliability"]
tags: ["request-id", "namespace-scope", "concurrency", "latent-bug", "isolation", "bug-161", "req-568"]
req: BUG-161
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

Tool-permission prompts were correlated by a `request_id` minted `perm-{n}` from
a counter on the **per-session** `PermissionGate` (each gate starts at 0), but
the pending-answer waiters lived in one **daemon-wide** map keyed by that id.
Two sessions each minted `perm-0`; the second `register` overwrote the first's
waiter. Answering one session's prompt then resolved the *other* session's tool
call, and the displaced waiter's dropped receiver denied a tool its user was
never asked about. The fix moved the counter onto the daemon-wide object that
owns the map, so mint and resolution share one namespace and every id is unique
by construction.

Two things kept this hidden. First, the id counter *felt* like session state, so
it was placed on the per-session gate — the natural home, and the wrong one.
Second, before REQ-568 every interactive client received every session's
`permission_request`, so a mis-keyed answer was masked by everyone answering
everything; REQ-568's session-scoped delivery removed that ambient broadcast and
turned a latent collision into the only outcome.

## Lesson

An identifier is only unique within the scope of the counter that mints it. If it
will be *resolved* in a wider scope — looked up in a shared map, matched across
sessions, echoed back by a client into a daemon-wide handler — it must be
*minted* at that wider scope. Put the counter next to the structure that resolves
the id, not next to the state that happens to create it. And treat any
isolation-tightening change (session-scoping, tenant-scoping, sandboxing) as a
latent-bug surfacer: broadcast and shared visibility routinely mask correctness
bugs by making a mis-route observable-and-answerable by someone; when you remove
the ambient path, audit what was silently relying on it. A defensive
`register` that *refuses to overwrite* (Entry API, log-and-drop) turns any future
re-introduction from a silent hijack into a loud, logged failure.

## Why It Matters

The masked outcome was a cross-session authorization: one user's "allow"
executed another user's tool call — a security defect, not a glitch — and it sat
on `main` through every prior release because the ambient broadcast hid it.
Pairs with [[LESSON-502]] (the verify pass that found it also found a monitor
drive-gap the tests missed): both are "the fix drew a new edge, and the edge is
where the bug lives."

## Applies When

Designing any correlation id, token, or handle that is generated in one scope
and consumed in another (per-request ids resolved by a shared dispatcher,
per-connection sequence numbers merged into a global log, per-tenant keys in a
shared cache); reviewing a change that narrows who-can-see-what (isolation,
scoping, permissioning) — enumerate what the old ambient visibility was masking.

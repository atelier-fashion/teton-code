---
id: BUG-161
title: "Permission request_ids collide across concurrent sessions, cross-authorizing tool calls"
status: open
severity: critical
created: 2026-08-11
updated: 2026-08-11
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["security", "reliability"]
tags: ["permission-gate", "request-id", "concurrency", "cross-session", "REQ-568"]
---

## Description

A tool-permission `request_id` is minted `perm-{n}` from a counter that lives on
the **per-session** `PermissionGate` (each `PermissionGate::new` starts its
counter at 0), but the pending waiters are stored in a single **daemon-wide**
map keyed by that `request_id`. Two sessions therefore both mint `perm-0`, and
`PendingPermissions::register` does a plain `HashMap::insert`, so the second
registration silently overwrites the first.

Found by the REQ-568 verify phase (correctness reviewer rated it Critical; the
reflector independently found the same collision). It is **pre-existing** — it
does not originate in REQ-568's diff — but REQ-568 makes it consequential:
before session-scoped delivery, every interactive client saw every session's
`permission_request`, so whoever answered masked the misrouting; after REQ-568
only the owning client sees its own request, so cross-session authorization
becomes the *only* outcome. REQ-568's own AC-1 runs two sessions concurrently by
design.

## Reproduction Steps

1. Client A prompts `sess-0`; its turn hits an `Ask` tool → gate A registers
   `perm-0` and publishes `permission_request{request_id:"perm-0"}` scoped to
   `sess-0`.
2. Client B prompts `sess-1`; its turn hits an `Ask` tool → gate B's
   `register("perm-0")` overwrites A's `oneshot::Sender` in the daemon-wide map.
3. A's dropped receiver takes the `Err` arm → A's tool is **denied without the
   user ever being asked** (misattributed to "client disconnected").
4. A's user answers the prompt still on their screen → `permission/respond
   {request_id:"perm-0"}` resolves **B's** waiter → session A's consent
   authorizes session B's tool call.

## Expected Behavior

A permission answer resolves exactly the request it was raised for, in the
session that raised it. Concurrent sessions never share a `request_id`.

## Actual Behavior

Concurrent sessions collide on `perm-0`, `perm-1`, …; one session's consent can
authorize another's tool call, or a tool is silently denied.

## Environment

- Platform: all
- Version: main @ REQ-568 branch (pre-existing on main before REQ-568)

## Root Cause

Per-session counter (`crates/tetond/src/harness/permissions.rs:407`, gate
constructed per session at `crates/tetond/src/runtime.rs:2646`) feeding a
daemon-wide waiter map (`PendingPermissions.waiters`,
`crates/tetond/src/harness/permissions.rs:208`). The id namespace is per-session
while the resolution namespace is daemon-wide.

## Resolution

(filled after fix) — candidate: make the id globally unique (daemon-wide
counter) or session-qualified (`perm-{session_id}-{n}`), and/or key the waiter
map on `(SessionId, RequestId)`; `register` should refuse/log on an id collision
rather than overwrite, so this class fails loudly. Fixing this is also the
prerequisite that lets `permission/respond` be attachment-gated (REQ-569): a
gate needs to resolve the request's owning session, which a session-qualified id
provides. Add a regression test that runs two concurrent sessions each raising a
permission request and asserts no cross-resolution.

## Files Changed

- `crates/tetond/src/harness/permissions.rs` — id minting + waiter keying (TBD)
- `crates/tetond/src/runtime.rs` — gate/waiter wiring (TBD)

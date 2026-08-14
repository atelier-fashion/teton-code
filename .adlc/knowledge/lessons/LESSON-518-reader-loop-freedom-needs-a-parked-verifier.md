---
id: LESSON-518
title: "A blocking gate's reader-loop freedom is not inherited from the await-based reader-loop tests"
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "tokio"]
concerns: ["reliability", "security", "developer-experience"]
tags: ["blocks-on-a-human", "block-in-place", "reader-loop", "presence", "br-10b", "test-coverage", "req-575"]
req: REQ-575
created: 2026-08-14
updated: 2026-08-14
---

## What Happened

REQ-575 moved `web/setup_commit` off the synchronous `dispatch` onto
`handle_client`'s `blocks_on_a_human` task, because it gained a presence gate
that blocks a human. The first coverage claim was that its "reader loop stays
free" property was *inherited* from the existing reader-loop tests for
`attach/consent` and `model/confirm`, plus a routing test proving the commit
left `dispatch`. The verify-pass review (test-auditor) refuted the inheritance:
those existing tests block the server on an **awaited client frame** (an async
`.await` that naturally yields the worker), whereas a real presence check blocks
**synchronously inside `verify()` under `tokio::task::block_in_place`** — a
different mechanism on a different code path. No test actually blocked inside
`refuse_unattested_commitment` and asserted a concurrent RPC was still served.

## Lesson

When a method moves onto the own-task/`blocks_on_a_human` path because it gained
a *synchronously blocking* gate, prove the liveness property with a test that
**actually blocks in that gate**: a `ParkingVerifier` whose `verify()` parks
(releasably, so the runtime can shut down), on a `#[tokio::test(flavor =
"multi_thread")]` runtime so the production `block_in_place` branch is exercised,
then assert a second RPC on the same connection is answered while the first is
parked. Make it non-vacuous with a channel the gate signals on entry, so the
concurrent RPC is provably served *after* the block began. A routing test
("the method is absent from `dispatch`") proves *membership*, never *liveness*.

## Why It Matters

The whole reason to move a method off the reader loop is that a synchronous
block would otherwise stall every other RPC on the connection. If that exact
failure mode is untested, a refactor that puts the method back on `dispatch`, or
a gate that blocks where the reviewer assumed it awaited, ships a
connection-wide stall with a green suite. `block_in_place` also panics on a
current-thread runtime, so an await-flavored test never touches the production
branch at all.

## Applies When

Adding a method to `blocks_on_a_human` (REQ-576's `config/set` is next), or any
time "runs off the reader loop" is the safety property — and the blocking is a
synchronous FFI/`block_in_place` call, not an `.await`. See [[lesson-508-a-redundant-guard-needs-its-own-test]] and [[lesson-502-a-multi-seam-invariant-needs-a-test-at-each-seam]].

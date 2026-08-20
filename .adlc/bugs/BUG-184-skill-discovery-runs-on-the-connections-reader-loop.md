---
id: BUG-184
title: "Skill discovery runs on the connection's synchronous reader loop, where a TCC dialog can park it"
status: open
severity: medium
created: 2026-08-20
updated: 2026-08-20
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "latency"]
tags: ["session-create", "set_cwd", "reader-loop", "block_in_place", "tcc", "req-585", "req-583"]
---

## Description

`rebuild_session_skills` (`crates/tetond/src/server.rs`) runs on the
**synchronous** `dispatch` path inside the per-connection reader task, for both
`session/create` and `session/set_cwd`.

Discovery is up to four `read_dir` calls plus, in the worst case,
`4 × MAX_ENTRIES_PER_ROOT = 2048` `metadata` + `open` + `read` calls of up to
64 KiB each — on user-controlled, symlinked paths.

The same `dispatch` function admits `skills/list` with an explicit note that it
is *"a read of a stored snapshot — no human, no network, no filesystem — so it
stays on the synchronous path"*, and `provider/test` was moved **off** it
because it "would park the reader loop for a round trip". Discovery meets
neither bar.

The REQ's own Assumptions say a root resolving under `~/Documents` "may raise a
one-time consent dialog" on macOS. A TCC dialog blocks the syscall for as long
as the user takes to answer it — on the reader loop of the connection that just
created the session, holding a tokio worker.

## Impact

A slow or consent-gated filesystem stalls the connection that issued
`session/create` or `/cd`, not just the request. The path already touched the
filesystem (`session_root_for`'s canonicalize), so this is a change of
magnitude and exposure rather than a new class — but the magnitude is the
point.

## Suggested fix

Wrap the discovery call in `tokio::task::block_in_place` — the pattern already
used at `crates/tetond/src/server.rs` for the verifier — or route
`session/create` and `session/set_cwd` through the existing spawned-task
branch. Record the choice in the architecture doc beside ADR-4's bounds: ADR-4
argues about *entry counts* and never about *where the I/O runs*.

## Found

REQ-585 Phase 5 verify (architecture review), 2026-08-20.

---
id: TASK-100
title: "Daemon: MAX_FRAME bounded request reader with refuse-then-close"
status: draft
parent: REQ-568
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

Cap inbound request frames at `MAX_FRAME` (4 MiB, ADR-D) so the per-connection
read buffer is incapable of exceeding the cap by construction (BR-6/AC-5). On
limit-hit: best-effort `INVALID_PARAMS` refusal with null id, then close the
connection (no resync — ADR-D resolves spec OQ-3).

## Files to Create/Modify

- `crates/tetond/src/server.rs` — (1) `const MAX_FRAME: u64 = 4 * 1024 * 1024;` with the ADR-D measurement comment (largest legitimate frame is session/prompt paste, observed well under 100 KiB; 4 MiB ≈ 40× headroom). (2) In the `handle_client` read loop, read via `(&mut reader).take(MAX_FRAME).read_line(&mut line)` (tokio `AsyncBufReadExt` over `AsyncReadExt::take`). Limit-hit detection: `read_line` returned `Ok(n)` with the buffer NOT ending in `\n` AND total bytes == MAX_FRAME → oversized frame. A non-newline-terminated final frame at EOF is distinguished by the subsequent `Ok(0)`. On oversized: `let _ = out_tx.try_send(error_string(Id::Null, error_code::INVALID_PARAMS, "frame exceeds maximum length"))` then `break` the loop (teardown path already aborts forwarder/turns). (3) CRITICAL correctness note: a fresh `take(...)` per loop iteration resets the budget per frame; do NOT construct the `Take` once outside the loop or the budget becomes per-connection-lifetime.
- `crates/tetond/tests/frame_cap.rs` — new integration test file with its own minimal raw-socket client (the multi_client.rs `TestClient` pattern; kept separate so Tier-1 parallelism stays disjoint from TASK-098's multi_client.rs edits). Tests: (a) a frame of MAX_FRAME+1 bytes (oversized JSON padding, newline-terminated) is answered by `-32602 INVALID_PARAMS` with `"id": null` and the connection then closes (subsequent read = EOF); (b) a fresh connection to the same daemon handshakes and serves normally (AC-5 recovery); (c) a legitimate large-but-legal frame (e.g. 64 KiB prompt) round-trips fine.

## Acceptance Criteria

- [ ] The reader cannot buffer more than MAX_FRAME bytes for a single frame — enforced by the `take` construction, and the review confirms no post-read length check stands in for it (the naive check is the defect, not the fix).
- [ ] Oversized frame → `INVALID_PARAMS`, null id, connection closed deterministically.
- [ ] Fresh connection afterwards serves normally; legal large frames unaffected.
- [ ] `cargo test -p tetond --test frame_cap` passes; existing tests unaffected.

## Technical Notes

- `Ok(0)` from a `take`-limited read is ambiguous (EOF vs exhausted budget) — disambiguate with the buffer-state rule above; write the unit-level reasoning as a comment where it lives.
- The refusal uses the existing `error_string(Id::Null, ...)` helper (same as the parse-error arm at ~server.rs:275).
- Do not touch `OUTBOUND_CAPACITY`, `TURN_DRAIN_TIMEOUT`, or broadcast capacities — out of scope.

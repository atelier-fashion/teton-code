---
id: TASK-162
title: "e2e harness: MockProvider answers an arbitrary status (401/404/429/5xx)"
status: complete
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: []
---

## Description

The e2e `MockProvider` can answer 200/400/500 today. The connection-test e2e
needs it to answer 401, 403, 404, 429 and 503 on demand — and to keep counting
requests so "nothing left the machine" stays assertable.

## Files to Create/Modify

- `crates/tetond/tests/e2e/harness.rs` — `MockResponse::status(code: u16)` (empty JSON body, or a small `{"error":{"message":"..."}}` body — the daemon must not render it, so any body works) and `MockResponse::status_with_body(code, body)`; `MockProvider::always_status(code)` convenience; keep `requests()` / `request_count()` recording for every status. A short doc block naming REQ-581 as the consumer.

## Acceptance Criteria

- [ ] A `MockProvider::always_status(401)` answers every request with HTTP 401 and records it in `requests()`.
- [ ] Existing e2e suites that use `MockProvider` are unchanged and green (`cargo test -p tetond --test e2e`, `--test remote_loop`, `--test routing`).

## Technical Notes

Look at how `bad_request()` / `always_bad()` are wired and generalize rather than duplicating; the writer thread already formats status lines. No header support is needed (architecture ADR-2).

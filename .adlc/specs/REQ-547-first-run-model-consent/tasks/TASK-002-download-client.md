---
id: TASK-002
title: "Production download client: credential-free, redirect-following RangeFetcher"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
dependencies: [TASK-001]
---

## Description

BR-13/BR-14. `RangeFetcher` currently has only `#[cfg(test)]` implementors and
`tetond` never constructs a `Downloader`, so no real download can happen. Build
the production fetcher with its own HTTP client — separate trust context from
egress (D-2).

## Files to Create/Modify

- `crates/tetond/src/download/mod.rs` — new module: the production `RangeFetcher` impl over a dedicated `reqwest::Client` built with **no default headers** and `redirect::Policy::default()` (follows HF `resolve` → CDN)
- `crates/tetond/src/download/backoff.rs` — 429/503 retry with exponential backoff + jitter, surfaced as a distinct rate-limit error (BR-16)
- `crates/tetond/src/lib.rs` — `pub mod download;`
- `crates/tetond/src/egress/mod.rs` — (read-only reference; do NOT relax its redirect policy)

## Acceptance Criteria

- [x] Implements the existing `teton_inference::RangeFetcher` byte-range contract; the library's resume/verify tests pass against it via a local test server
- [x] The download client sends **no** `Authorization`/`x-api-key`/any auth header on any request — asserted, not assumed
- [x] The download client follows a 302 to a different host (mimicking HF → CDN); the egress client still **refuses** redirects — both asserted in one test module so relaxing one fails the other (AC-11)
- [x] HTTP 429 and 503 retry with backoff and are reported as rate-limit/availability, distinct from a corrupt-download error (AC-12 partial)
- [x] `deny_http_client` still passes (this client lives in tetond, the only permitted crate)
- [x] Tests: local server exercising range/resume, redirect-follow, auth-header absence, 429 backoff, and the egress-still-refuses counterpart

## Technical Notes

Do NOT reuse `egress::HttpTransport` — that is the whole point of D-2. A model
fetch carries no user content and no credential; keeping the clients separate is
what prevents a future "just allow redirects" change from re-opening the
credential-forwarding hole the egress policy closed.

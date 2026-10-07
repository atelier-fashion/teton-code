---
id: TASK-423
title: "e2e fixture: MockProvider request matching and a rendezvous hold"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: []
repo: teton-code
---

## Description

ADR-6. `MockProvider` serves scripted replies in arrival order; concurrent children make arrival
order a scheduler accident. Add `MockProvider::start_matching(Vec<(Matcher, MockResponse)>,
default)` where a `Matcher` is a request-body substring, and `MockResponse::rendezvous(n)` —
held until `n` requests are parked on it, then released to all. This is the primitive AC-5
and AC-6 are written against, and what every later concurrent-child test addresses children
with.

## Files to Create/Modify

- `crates/tetond/tests/e2e/harness.rs` — `Matcher`, `start_matching`, `rendezvous`, the parked-request accounting; keep `start`/`start_delayed` byte-identical in behaviour
- `crates/tetond/tests/e2e/harness_tests.rs` — new: fixture self-tests (a matched reply reaches the matching request; a rendezvous of 2 releases both; a rendezvous of 2 with one request parks forever and the test times out on purpose)

## Acceptance Criteria

- [x] Two concurrent requests with different task text each receive their matched reply regardless of arrival order
- [x] `rendezvous(3)` holds two requests and releases all three when the third arrives
- [x] Existing e2e suites using `start`/`start_delayed` are unchanged and green
- [x] `global_capture` and `assert_no_boundary_bytes` still see every request body on the matching path

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-5 | test-case | `crates/tetond/tests/e2e/harness_tests.rs::rendezvous_releases_when_n_parked` | yes |
| AC-6 | test-case | `crates/tetond/tests/e2e/harness_tests.rs::matched_reply_reaches_matching_request` | no |

## Technical Notes

- The mock is a plain `thread`-backed HTTP server (`harness.rs:276-320`); a rendezvous needs a `Condvar` or a counting barrier per held response, not tokio.
- LESSON-540: do not depend on which parked request is released first; release all, assert the set.
- Test the fixture's own failure mechanism before building AC-5 on it (conventions "verify the failure mechanism before building a fixture around it").

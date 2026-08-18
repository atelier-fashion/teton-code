---
id: TASK-167
title: "e2e: provider_test_flow over the socket (reached / 401 / 404 / 429 / 5xx / closed port / NOT_ATTACHED); CHANGELOG"
status: draft
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: [TASK-162, TASK-164]
---

## Description

Prove the whole chain on the real daemon binary against the e2e harness's mock
provider, and record the change.

## Files to Create/Modify

- `crates/tetond/tests/provider_test_flow.rs` (new; register it the way sibling suites are — check whether `tests/e2e.rs` mounts modules or each `tests/*.rs` is its own target) — spawn a daemon with a config registering `probe` (openai-compatible) at `MockProvider::openai_endpoint()`, `auth_ref` an `env:` reference the daemon can resolve in the test env (see how `provider_setup_flow.rs` / `remote_loop.rs` supply a key without a keychain); cases: (a) mock 200 with an `openai_turn` body carrying usage → `outcome.outcome == "reached"`, `input_tokens/output_tokens` present, one `provider_tested` event on the creator's connection scoped to its session, `cost/query` shows `probe_calls == 1` and one `probe` provider row; (b) `always_status(401)` → `refused`, `status 401`, `reason` contains the auth_ref and NOT the key value (assert against the env value), no ledger row; (c) 404 → `unknown_model` naming the model; (d) 429 → `rate_limited`; (e) 503 → `server_error`; (f) endpoint on a closed port → `unreachable`; (g) a second connection that did not attach the session → `NOT_ATTACHED` and `mock.request_count()` unchanged; (h) a `kind = "local"` provider id → `INVALID_PARAMS`, `request_count()` unchanged.
- `CHANGELOG.md` — `[Unreleased]` → `### Added`: `/provider test <id>` / `teton provider test <id>` (what it sends, what it says, that it moves routing health and is billed as a probe), and the session hand-off.

## Acceptance Criteria

- [ ] `cargo test -p tetond --test provider_test_flow` green, all eight cases; each failure case asserts the typed `outcome` tag, not prose.
- [ ] Case (a) also asserts health: a follow-up `session/prompt` on an `edit`-class turn routes to `probe` (`route_decided.provider_id == "probe"`) — AC-5's end-to-end half — or, if a mock-served turn is impractical here, the runtime unit test in TASK-163 is named as the AC-5 evidence and this test asserts `health_after == "healthy"`.
- [ ] `cargo test --workspace --no-fail-fast` green; clippy + fmt clean.

## Technical Notes

`MockResponse::status(...)` comes from TASK-162; `openai_turn(...)` builds a 200 body with usage. For the closed port use the `closed_port()` helper pattern from consent_matrix.rs. Keep the key value used in the test distinctive (e.g. `sk-PROBETESTKEY…`) so the "never printed" assertions are non-vacuous (LESSON-519).

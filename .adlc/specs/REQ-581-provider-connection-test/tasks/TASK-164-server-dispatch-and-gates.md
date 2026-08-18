---
id: TASK-164
title: "Daemon server: dispatch provider/test on its own task behind the session gates"
status: draft
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: [TASK-163]
---

## Description

Wire `provider/test` into the server: parsed, gated like the setup trio's reads
(`refuse_unmintable_session_id`, then `may_drive` → silent `NOT_ATTACHED`, no
event — LESSON-513), run off the reader loop (it blocks on the network), and
answered through the event fence.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — add `ProviderTestParams::METHOD` to the own-task method list (the `blocks_on_a_human` `matches!` guard and its branch chain; update the guard's comment to say "a human or the network"); `async fn handle_provider_test(daemon, conn, id, params) -> String`; tests in the server test module: a foreign (unattached) connection gets `NOT_ATTACHED` and no event is published and the runtime's provider was never dialed (assert through a `MockProvider`/`request_count() == 0` or by using an endpoint on a closed port and asserting no `provider_tested` event); the session's own creator gets a `provider_tested` event scoped to the session; a tool-spawned caller is covered by the ancestry-descendant exclusion already tested for `session/prompt` — add one assertion mirroring `spawn_prompt_turn`'s monitor/stranger tests for this method.

## Acceptance Criteria

- [ ] `provider/test` from a connection that did not attach the session → `NOT_ATTACHED` in-response, no event, zero requests to the provider (covers AC-6).
- [ ] From the session's creator → runs, answers `ProviderTestResult`, publishes one `provider_tested` scoped to that session.
- [ ] The reader loop stays free while the test runs (place the branch in the own-task list; a unit test that issues a second RPC while a slow mock provider holds the first — LESSON-518's shape — is required, not optional).
- [ ] `cargo test -p tetond --lib server` green.

## Technical Notes

Copy `handle_provider_setup_plan`/`preview` for the read gates and `handle_provider_setup_commit`'s own-task placement; do NOT add presence attestation (no config change) and do NOT publish a rejection event on refusal (a probe refused announces nothing — the commit-only rule). `refuse_commit_without_session_access` publishes an event when the session exists; use the plan/preview-style silent variant instead.

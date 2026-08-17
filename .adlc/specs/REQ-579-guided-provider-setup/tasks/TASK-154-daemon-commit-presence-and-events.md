---
id: TASK-154
title: "Daemon: provider_setup_commit — one digest-bound write, live re-derive, presence gate, events"
status: complete
parent: REQ-579
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-153"]
---

## Description

Implement the commit. It calls TASK-153's `derive_provider_setup` again, compares the digest to `expect_digest`, and if equal hands the digested bytes to the writer as-is (not `persist_config` — see the `web_setup_commit` comment block on why a digest-checked write must not re-read the file). Then it re-runs the startup load/validate/derive path so routing is live in the committing session with no restart (REQ-572 BR-8). Returns `applied: false` when the current config already equals the candidate. The handler is async-spawned like `web/setup_commit`, sits behind `refuse_unattested_commitment` after `may_drive`, is added to `COMMITMENT_METHODS`, and emits `ProviderSetupCompleted` on success and `ProviderSetupRejected` (wire `provider_setup_rejected_nonuser`) when a model tool call or foreign connection reaches it.

**Covers:** AC-2 (live re-derive), AC-4 (event payload carries no key), AC-10, AC-11, AC-12 (comment-preserving replace)

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `pub fn provider_setup_commit(&self, c: &ProviderSetupCandidate, expect_digest: Option<&str>) -> Result<ProviderSetupCommitResult, RpcError>`; digest mismatch → `PROVIDER_SETUP_INVALID` with a sentence naming "the preview you confirmed no longer matches"; single write of the digested bytes; live re-derive
- `crates/tetond/src/server.rs` — `handle_provider_setup_commit`; add `ProviderSetupCommitParams` to the async-spawn router beside `WebSetupCommitParams`; wire `refuse_unattested_commitment` after `may_drive`; add `ProviderSetupCommitParams::METHOD` to `COMMITMENT_METHODS`; publish `ProviderSetupCompleted` to connected clients on success; publish `ProviderSetupRejected` on the nonuser refusal (commit only — plan/preview stay in-response, LESSON-513)
- `crates/tetond/src/harness/tools/mod.rs` (or wherever model tool names are matched against RPC methods) — ensure a model tool call named `provider/setup_commit` (and plan/preview) is refused with `NOT_ATTACHED` (the existing code `web/setup_*` uses for a foreign caller — there is no `SETUP_REJECTED_NONUSER`) — verify how `web/setup_commit` achieves this and copy the exact mechanism
- `crates/tetond/src/server.rs` (tests) — extend the presence-gate test family (~L4179–4250) so the `COMMITMENT_METHODS` loop covers the new method; add `a_provider_setup_commit_refuses_when_the_presence_check_fails` under `TETON_PRESENCE_ACCEPT=fail`; add a test that a commit from a connection that did not open the session is refused with `NOT_ATTACHED` (the existing code `web/setup_*` uses for a foreign caller — there is no `SETUP_REJECTED_NONUSER`) AND the config bytes on disk are unchanged AND the rejected event was published
- `crates/tetond/src/runtime.rs` (tests) — commit happy path writes exactly the preview bytes and the next routing decision for `think` resolves to the new id with no restart; commit with a stale digest refuses and writes nothing (assert file bytes unchanged — LESSON-519); commit of an unchanged candidate returns `applied: false` and writes nothing; commit that replaces an existing `kimi` keeps every other `[[providers]]` row and every comment byte-identical (REQ-574)

## Acceptance Criteria

- [ ] `provider/setup_commit` from the session's own connection with a matching digest writes once, returns `applied: true`, routing is live in-session, `provider_setup_completed` is delivered
- [ ] Stale digest, foreign connection, and model-tool-call paths each write nothing (asserted by reading the file, not by absence of error) and return the specified codes; the nonuser path also emits `provider_setup_rejected_nonuser`
- [ ] `TETON_PRESENCE_ACCEPT=fail` refuses the commit exactly as it refuses `web/setup_commit`; on a build without the presence feature the commit degrades exactly as `web/setup_commit` does — no new prompt, no more permissive
- [ ] `COMMITMENT_METHODS` includes the new method and the loop test exercises it
- [ ] `cargo test -p tetond` green; clippy clean

## Technical Notes

`web_setup_commit` (runtime.rs ~L4093) is the template — copy its ordering exactly: derive → digest check → unchanged short-circuit → write digested bytes → re-derive. The re-derive seam is whatever `web_setup_commit` calls after the write (search for the call that follows the write). Presence: `refuse_unattested_commitment` (server.rs ~L974–1038) — call it with the same request-id/connection binding `handle_web_setup_commit` uses (~L2489–2503). Event publish: copy `WebSetupCompleted` publish; the payload carries no key and no endpoint (BR-2). The `daemon_wide_methods()` test helper (~L4193) is for ancestry-gated daemon-wide methods — provider setup is *session-scoped* like web setup, so it does NOT go there; add a one-line comment saying so next to the `COMMITMENT_METHODS` addition to stop a future reader "fixing" it.

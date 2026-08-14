---
id: TASK-129
title: "Runtime: plan/preview/commit seams, candidate validation, config swap, events"
status: draft
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-127", "TASK-128"]
---

## Description

The daemon-side substance (architecture ADR-2): three runtime methods backing
the setup RPCs, the candidate-config validation path, the atomic write +
in-memory swap commit, the `WebSetupCompleted`/`WebSetupRejected` events, and
the `capability_dead_end` emission at the unserved-turn remote-tier path
(architecture ADR-4).

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `pub fn web_setup_plan(&self, session_id) -> WebSetupPlanResult` (derive state via `web_capability_state(&config.web, self.engine-present-predicate)`; reuse whatever predicate `search_redaction_gate`/BR-14 already uses for "local model present" — one classifier, LESSON-456); `pub fn web_setup_preview(&self, params) -> Result<WebSetupPreviewResult, RpcError>`: clone current config, apply params to `.web`, run `Config::validate()` on the candidate (failures → `WEB_SETUP_INVALID` carrying the validator's sentence), derive `search_host` from the executor's parse (the same `origin_of`/`reqwest::Url` path `search_auth` uses — LESSON-494), render `web_table_toml(candidate.web)`, attach warnings (search selected while SearchUnavailable is REFUSED, not warned — AC-7); `pub fn web_setup_commit(&self, params) -> Result<WebSetupCommitResult, RpcError>`: rebuild the candidate from params (never trust a client-side preview), re-validate, serialize the FULL document via `Config::to_toml()`, write via the `persist_web_tier` atomic pattern (runtime.rs:3755), swap `*self.config.lock()`, publish `WebSetupCompleted` session-scoped.
- `crates/tetond/src/runtime.rs` — in `unserved_turn_error`: when the unserved cause is "remote tier wanted, none configured", publish `capability_dead_end`-shaped telemetry as the existing event vocabulary allows (add `Event::CapabilityDeadEnd { capability: String }` to TASK-127's event set if not already there — coordinate; session-scoped).
- `crates/tetond/src/runtime.rs` — unit tests beside the existing `persist_web_tier` tests: commit writes bytes equal to preview's rendering for identical params; a candidate failing validation writes nothing and leaves the mutex config untouched; the swap makes the very next `build_tools` register the web tool (assert via a follow-up registry build in-test).

## Acceptance Criteria

- [ ] Preview and commit derive from one candidate-construction function; a test asserts preview `toml` equals the `[web]` section of the bytes commit writes
- [ ] A validation failure at commit leaves config.toml byte-identical (read-back assertion) and the in-memory config unchanged
- [ ] After a successful commit, a `build_tools`-produced registry contains the web tool without any restart, and `web_setup_plan` reports Ready — in the same test process
- [ ] `WebSetupCompleted` publishes with `session_id = Some(committing session)`; no event on failed validation
- [ ] The remote-tier unserved turn publishes the dead-end event; the four settled `UNKNOWN_PROVIDER` causes keep their code (BUG-152 regression guard untouched)

## Technical Notes

`runtime.rs` is 16k+ lines — navigate by symbol (`persist_web_tier`,
`unserved_turn_error`, `build_tools`, `search_auth`), never read whole. The
"local model present" predicate must be the one BR-14 search-gating already
consults — grep how the search redaction gate decides the local tier exists
and reuse that exact call. Do not introduce a second config-write helper:
extend/share the `persist_web_tier` write body (extract a private
`write_config_atomically(&self, config: &Config)` if needed so both callers
share one seam).

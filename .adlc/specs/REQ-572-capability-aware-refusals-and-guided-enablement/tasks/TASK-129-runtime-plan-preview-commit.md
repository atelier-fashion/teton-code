---
id: TASK-129
title: "Runtime: plan/preview/commit seams, candidate validation, config swap, events"
status: complete
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

- [x] Preview and commit derive from one candidate-construction function; a test asserts preview `toml` equals the `[web]` section of the bytes commit writes — `DaemonRuntime::web_setup_candidate` is the single construction (both methods call it and nothing else builds a candidate); `a_preview_renders_the_bytes_the_commit_goes_on_to_write` asserts the equality over three shapes (fetch-only, keyless search, search with key_ref + auth template), reading the bytes back off disk
- [x] A validation failure at commit leaves config.toml byte-identical (read-back assertion) and the in-memory config unchanged — `a_candidate_that_would_not_load_writes_nothing_and_moves_nothing` (file bytes, `config.web`, and "no event" in one test; plus the same answers refusing identically at preview)
- [x] After a successful commit, a `build_tools`-produced registry contains the web tool without any restart, and `web_setup_plan` reports Ready — in the same test process — `a_committed_tier_reaches_the_next_registry_and_the_next_plan`, with the before-state asserted for non-vacuity
- [x] `WebSetupCompleted` publishes with `session_id = Some(committing session)`; no event on failed validation — `a_commit_announces_itself_to_the_committing_session` (scope, tier, config_path, exactly one event); the no-event half is asserted at the failed-validation test above and again at `a_commit_that_changes_nothing_applies_nothing_and_announces_nothing`
- [x] The remote-tier unserved turn publishes the dead-end event; the four settled `UNKNOWN_PROVIDER` causes keep their code (BUG-152 regression guard untouched) — `the_unserved_remote_turn_announces_a_dead_end_only_when_there_is_one`: announced with nothing configured (and the classifier's code *and* sentence asserted byte-equal to `unserved_turn_error`'s), silent for a configured-but-unrouted remote tier, silent for a `TIER_WARMING` machine. `unserved_turn_error_names_the_state_that_actually_applies` is untouched and still green

## Technical Notes

`runtime.rs` is 16k+ lines — navigate by symbol (`persist_web_tier`,
`unserved_turn_error`, `build_tools`, `search_auth`), never read whole. The
"local model present" predicate must be the one BR-14 search-gating already
consults — grep how the search redaction gate decides the local tier exists
and reuse that exact call. Do not introduce a second config-write helper:
extend/share the `persist_web_tier` write body (extract a private
`write_config_atomically(&self, config: &Config)` if needed so both callers
share one seam).

## Implementation notes (as built)

**Surfaces added.** `crates/tetond/src/runtime.rs`:
`DaemonRuntime::{web_setup_plan, web_setup_preview, web_setup_commit}` (public,
for TASK-130's dispatch), the private `web_setup_candidate` /
`local_model_present` / `search_tier_gap` / `unserved_turn_error_announcing`,
and the module-level `WebSetupAnswers` / `setup_answer` /
`to_protocol_capability_state` / `web_table_summary` / `web_setup_warnings` /
`has_remote_provider`. `crates/tetond/src/egress/lookup.rs`:
`from_protocol_web_tier` (re-exported from `egress`).
`crates/teton-protocol/src/events.rs`: `CapabilityDeadEnd::{REMOTE_PROVIDER,
WEB_SEARCH}`.

Deviations from the letter of this file, each with its reason:

1. **`web_setup_plan(&self)` takes no `session_id`.** The plan reads config and
   the engine slot and nothing per-session; attachment is the gate's question
   and the gate is TASK-130's (it holds the params). A parameter the body never
   reads would have been a dead argument dressed as an authorization check.
2. **The "local model present" predicate is `EngineSlot::present()`**, the same
   slot `RedactionGateImpl::redact_route` reads per scan and the same one
   `build_router`/`run_one_attempt` already consult — *not* a second engine
   probe. Deliberately **not** also re-resolving `Category::Redact`: that half
   of the gate's condition answers *which provider serves the scan*, needs a
   `Router` a config read has no business building, and would have added a
   second `Category::Redact` routing call site for the `call_sites` scan to
   account for. Recorded here because the omission is a judgement, not an
   oversight.
3. **No new write helper was needed.** `write_config_atomically(path, &config)`
   was already a shared free function (`persist_web_tier` calls it), so the
   commit uses it as-is — there is still exactly one config-write body. The
   commit holds the config mutex across build → validate → write → swap, as
   `persist_web_tier` does, which is what makes concurrent commits serialize
   (ADR-1) and what makes "a failed commit leaves the mutex untouched" true by
   construction: the assignment is the last statement.
4. **`search_host` comes from `crate::web::canonical_host_of`, not `origin_of`.**
   Both are the executor's `reqwest::Url` parse; they answer different
   questions. `origin_of` yields `scheme://host:port` and is what *binds the
   credential*, so it is used verbatim in the preview **warning** that mirrors
   `search_auth`'s fail-closed check. The protocol field is documented as a
   host, and the host is what the lookup seam records for a search destination
   (`lookup::host_of` → `canonical_host_of`), so that is what the confirm step
   shows.
5. **The AC-7 refusal is derived, not restated.** Preview and commit both refuse
   through `web_capability_state(&candidate.web, local_model_present)` returning
   `SearchUnavailable`, and the message carries `SearchGap::as_str()` verbatim —
   so the menu, the refusal and the tool's own behaviour cannot come apart.
   Likewise `search_tier_gap` asks the classifier about a probe table rather
   than spelling `!local_model_present()` a second time.
6. **`from_protocol_web_tier` reverses a bridge whose doc said "deliberately
   one-way".** That doc paragraph was amended rather than ignored: a tier in a
   setup param is an answer the user typed, and it becomes a *candidate* that
   `Config::validate` accepts or refuses exactly as it would the same table read
   from disk — the ceiling is not raised by assertion.
7. **`unserved_turn_error` itself is byte-for-byte unchanged** except for one
   line reading `has_remote_provider(config)` instead of the inline `any(…)`
   (the announcement keys on the same fact; two spellings is how they drift).
   The announcement lives in a thin wrapper and fires only when the code is
   `UNKNOWN_PROVIDER` **and** no remote provider is configured — so BUG-152's
   `TIER_WARMING` arms announce nothing, and a configured-but-broken remote tier
   announces nothing either (its remedy is already in the sentence).
8. **`ConfigSnapshot.web_capability` is now always `Some`**, filled by
   `snapshot_from_config`, which gained a `local_model_present: bool` argument
   (the one runtime fact it cannot read off a `Config`). The field stays
   `Option` on the wire because a *client* may be talking to an older daemon.
9. **Preview warnings** (the protocol's `warnings` list, previously unfilled):
   an endpoint written below the `search` tier, a `search` tier with no key
   reference, a key reference the endpoint cannot bind to (or with no endpoint
   at all), and — because the candidate is a re-derivation and not a patch — the
   keys the answers drop, named before the write rather than discovered after
   it.
10. **`CapabilityDeadEnd::WEB_SEARCH` is added but unused here.** TASK-131 owns
    the tool-side tier-gap emission; the id is defined beside `REMOTE_PROVIDER`
    so the two emission sites cannot ship two spellings of one capability.

**Suite state at commit.** `cargo test -p tetond --lib` is green (1232 passed).
Two failures elsewhere in `-p tetond` belong to TASK-131's concurrent work in
this same worktree — `tests/web_lookup_egress.rs` pins the *old*
`WEB_OPT_IN_CLAUSE` wording that their `turn_loop.rs` change replaces, and their
`harness/tools/web.rs` was mid-edit. Neither file is in this task's scope and
neither is touched by this commit; nothing in this diff produces a system
prompt.

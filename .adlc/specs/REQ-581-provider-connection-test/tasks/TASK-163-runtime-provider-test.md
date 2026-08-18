---
id: TASK-163
title: "Daemon runtime: provider_test — one minimal call through egress, typed outcome, health, ledger, event"
status: draft
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: [TASK-160, TASK-161]
---

## Description

The method itself: `DaemonRuntime::provider_test(&self, events, session_id,
provider_id) -> Result<ProviderTestResult, RpcError>` per architecture ADR-1..3
and ADR-5's session requirement.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `pub async fn provider_test(...)`: (1) look up the provider in the config snapshot; unknown id → `INVALID_PARAMS` naming the registered ids; `kind = "local"` → `INVALID_PARAMS` whose message is the local tier's state (reuse `unserved_turn_error(&config, None).message` or `local_tier_state()`; BR-8) — no call made; (2) `build_provider(provider, CapabilityProfile::from_core(caps))` and `build_remote_transport(provider, &self.secret_resolver)` — a `Credential` error → `CONFIG_REJECTED` "the credential reference `<auth_ref>` could not be resolved: <reason>. Nothing was sent."; (3) `Egress::new(transport, config.boundaries.clone(), events.clone()).with_cost_meter(Arc::new(self.ledger.clone()))` — no redaction gate (constant payload; say so in a comment); (4) `TurnRequest { model, system: None, messages: [User: PROBE_PROMPT], tools: [], max_tokens: PROBE_MAX_TOKENS, effort: <the omitted/none resolution the router uses when no effort is configured — read how `run_one_attempt` builds it and pick the "no field sent" value> }`; `CostAttribution::new(model).probe()`; `EgressContext::new(provider_id).with_session(session_id).with_cost(attribution)`; `egress.scoped(Provenance::empty(), ctx)`; `Instant::now()` before `stream_turn`, drain the stream (drop text, keep `Completed(usage)`), latency at stream end; (5) map to `ProviderTestOutcome` per ADR-2's table (a helper `fn probe_outcome(err: &ProviderError, host: &str, model: &str, auth_ref: Option<&str>) -> ProviderTestOutcome`, unit-tested as a table); `usd_micros` for `Reached` from the ledger's price table when priced (read the row the meter just wrote, or price via the same table — pick the one that cannot disagree with `teton cost`); (6) health: `Reached` → `record_health(id, HealthRecord::healthy())`; failure with a `failure_class()` → `health_record_after_failure(class, Instant::now())` → `record_health` if `Some`; `health_after` read back from `health_snapshot()` (map to the wire `ProviderHealth`); (7) `dial_host` via `crate::web::canonical_host_and_port_of(endpoint)` (fallback: the endpoint's origin); (8) `events.publish(Some(session_id), Event::ProviderTested { provider_id, outcome, health_after })`; return `ProviderTestResult`. Constants `PROBE_PROMPT` ("Reply with the single word OK.") and `PROBE_MAX_TOKENS` (8) with doc comments.
- `crates/tetond/src/runtime.rs` (tests) — `mod provider_test`: outcome mapping table (each `ProviderError` variant/status → variant; 404 → `UnknownModel` naming the configured model; 401 reason contains the auth_ref and never a secret string); local-kind refusal makes no transport; unknown id; a `MockProvider`-free unit path: use the crate's `Transport` test doubles (see `harness/duty.rs` shared fixtures) to return 200-with-usage / 401 / 429 / transport error and assert outcome + `health_snapshot()` movement (`Unavailable` seeded → `Healthy` after `Reached`; failure stamps a cooldown) + ledger row `probe = true` on `Reached` and no row on 401 + one `provider_tested` event on the bus scoped to the session.

## Acceptance Criteria

- [ ] `cargo test -p tetond --lib provider_test` green; every row of ADR-2's mapping table is asserted.
- [ ] After a `Reached` outcome the ledger's `report().probe_calls == 1` and the row's `provider_id`/`model` match; after a `Refused` outcome the ledger is unchanged.
- [ ] `Unavailable` health becomes `Healthy` after `Reached` (AC-5's daemon half); a 401 leaves health per `health_record_after_failure`'s verdict.
- [ ] No test or code path reads a response body or header into a `reason`.

## Technical Notes

Read `RemoteDuty::perform` (harness/duty.rs ~837) for the request/attribution/scoped-transport shape and `run_one_attempt` (~3429+) for the constructors; do NOT call the turn loop and do NOT retry or fall back (ADR-1). Reason sentences: "HTTP 401 from `<host>` — the vendor did not accept the credential at `<auth_ref>`" / "`<model>` is not a model `<host>` serves (HTTP 404)" / "rate limited by `<host>` (HTTP 429); try again shortly" / "HTTP 5xx from `<host>`" / "could not reach `<host>`: <timeout|transport|malformed response>". Never format the secret; `provider_auth_headers` builds headers from it and that stays inside `build_remote_transport`.

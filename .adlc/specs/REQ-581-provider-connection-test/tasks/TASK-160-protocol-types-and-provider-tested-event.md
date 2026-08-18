---
id: TASK-160
title: "Protocol: provider/test params + typed outcome + provider_tested event + additive probe fields"
status: draft
parent: REQ-581
created: 2026-08-17
updated: 2026-08-17
dependencies: []
---

## Description

Add the wire vocabulary for the connection test to `teton-protocol`, additively:
the `provider/test` method's params/result, the tagged `ProviderTestOutcome`
enum, the session-scoped `provider_tested` event, and the two additive fields
the ledger/report need (`CostRecord.probe`, `CostReportView.probe_calls`).

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — `ProviderTestParams { session_id, provider_id }` with `impl RpcMethod` (`METHOD = "provider/test"`, `Result = ProviderTestResult`); `ProviderTestOutcome` (`#[serde(tag = "outcome", rename_all = "snake_case")]`: `Reached { latency_ms: u64, input_tokens: u64, output_tokens: u64, usd_micros: Option<i64> }`, `Refused { status: u16, reason: String }`, `UnknownModel { status: u16, reason: String }`, `RateLimited { retry_after_secs: Option<u64> }`, `ServerError { status: u16, reason: String }`, `Unreachable { reason: String }`); `ProviderTestResult { provider_id: ProviderId, model: String, dial_host: String, outcome: ProviderTestOutcome, health_after: ProviderHealth }` (reuse the existing wire `ProviderHealth` type if one exists in the protocol crate; otherwise add a `#[serde(rename_all = "snake_case")]` enum `ProviderHealth { Healthy, Degraded, Unavailable }` beside `CostReportView`); `CostReportView.probe_calls: u64` with `#[serde(default)]`; unit tests: round-trip every outcome variant, the wire tag names, `METHOD` literal, and old-shape tolerance for the two additive fields.
- `crates/teton-protocol/src/events.rs` — `Event::ProviderTested(ProviderTested)` (`provider_tested`); `ProviderTested { provider_id, outcome: ProviderTestOutcome, health_after }`; add to `Event::name()`; add a row to `event_names_match_the_spec_events_table`; add a session-scoped wire test in the shape of `the_provider_setup_events_are_session_scoped_under_their_wire_names` asserting the exact key set (no `session_id` in the payload — the envelope carries it); module-doc index sentence ("REQ-581 adds `provider_tested`"). `CostRecord.probe: bool` with `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` and a doc comment (a probe is a connection test, billed like any call, counted apart).

## Acceptance Criteria

- [ ] `cargo test -p teton-protocol` green; new tests cover: each `ProviderTestOutcome` variant round-trips and serializes under its snake_case tag; `ProviderTested` envelope has keys exactly `event, health_after, outcome, provider_id, seq, session_id`; a `CostRecord` JSON without `probe` deserializes with `probe = false`; a `CostReportView` JSON without `probe_calls` deserializes with `0`; `Event::name()` for the new variant is `"provider_tested"`.
- [ ] `event_names_match_the_spec_events_table` includes the new variant.
- [ ] Nothing else in the workspace changes shape (additive only); `cargo check --workspace` green.

## Technical Notes

Copy REQ-579's `ProviderSetup*` types for doc voice and test shape (methods.rs ~1868+, events.rs ~1672+). `reason` strings are the daemon's own sentences (architecture ADR-3) — say so in the doc comment so nobody later "improves" it with a response body. Keep `retry_after_secs` on `RateLimited` even though v1 always sends `None` (ADR-2/OQ-5). Check whether `ProviderHealth` already exists in `teton-protocol` (`route_decided`/`provider_degraded` carry health words) before adding one — reuse if so.

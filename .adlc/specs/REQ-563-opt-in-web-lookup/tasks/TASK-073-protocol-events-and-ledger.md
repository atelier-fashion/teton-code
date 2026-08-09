---
id: TASK-073
title: "Protocol web events + append-only web_lookups ledger table"
status: complete
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: []
---

## Description

Add the wire vocabulary (architecture D-8) and the ledger sibling table
(architecture D-7). Pure additive definitions — no daemon behavior yet.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — three new `Event` variants: `WebLookup { kind: fetch|search, host: String, outcome: WebLookupOutcome, bytes_in: u64 }` with `WebLookupOutcome` ∈ {completed, cache_hit, blocked_privacy, blocked_redact, refused_domain, refused_tier, taint_restricted, offline}; `WebConsentDecided { scope: once|session|persistent, tier, granted: bool }`; `WebTaintOverridden { tiers_restored }`. Add to `Event::name()` match (snake_case names `web_lookup`, `web_consent_decided`, `web_taint_overridden`) and the index comment at the top of the enum.
- `crates/teton-protocol/src/methods.rs` — request/response types for the two client RPCs: `web/override` (no params → ack with tiers restored) and `web/refresh` (url → ack evicted|absent). Types only; handlers land in TASK-077.
- `crates/tetond/src/cost/ledger.rs` — `web_lookups` table: `id, ts, session_id, kind, host, bytes_in, duration_ms, outcome, usd_micros` with the same append-only UPDATE/DELETE-denying trigger pattern as the provider table (ledger.rs:65-70); `record_web_lookup()` insert fn; extend the `/cost` aggregation query to include lookup counts + bytes per session.

## Acceptance Criteria

- [ ] Each new event round-trips serde (tagged `event` snake_case, envelope-wrapped) — mirror the existing event serde tests.
- [ ] `Event::name()` covers all three; the enum index comment lists them (integration finding: "index, not decoration").
- [ ] Events carry host only — never full URL, query text, or credentials (spec BR-7 surface of charter BR-7; assert in test that the types have no such fields).
- [ ] `web_lookups` rejects UPDATE/DELETE via trigger (test parity with the provider-rows trigger test).
- [ ] `/cost` aggregation returns lookup count + total bytes for a session with recorded rows.

## Technical Notes

- Outcome enum folds the spec's separate blocked events; the mapping is
  documented in architecture D-8 — keep variant doc-comments naming which spec
  event each outcome realizes.
- `usd_micros` stays 0 for MVP but the column exists so metered search APIs
  later need no migration.

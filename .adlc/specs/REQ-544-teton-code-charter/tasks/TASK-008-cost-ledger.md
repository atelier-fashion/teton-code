---
id: TASK-008
title: "tetond cost ledger: CostRecord per call, attribution, savings baseline"
status: draft
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-005, TASK-007]
---

## Description

BR-2 backend: every remote call produces a CostRecord (session, phase,
provider, model, tokens, usd) recorded at the egress hook; aggregation queries
power AC-4's meter; savings-vs-frontier baseline computation (OQ-6 gets its
first concrete answer here).

## Files to Create/Modify

- `crates/tetond/src/cost/ledger.rs` — append-only store (SQLite via rusqlite, daemon-local file); CostRecord write at egress completion; emits `cost_recorded`
- `crates/tetond/src/cost/prices.rs` — provider price table (versioned TOML data file, like the model catalog); unknown model → record tokens with usd=null, never guess
- `crates/tetond/src/cost/report.rs` — aggregations: per-session, per-phase, per-provider; savings estimate = same token volume priced at configured "baseline" frontier model
- `crates/tetond/tests/cost_attribution.rs` — every egress call in a scripted session yields exactly one CostRecord with correct (session, phase) attribution

## Acceptance Criteria

- [ ] One CostRecord per completed remote call; none for local-tier inference; retries recorded individually (BR-2)
- [ ] Meter derives only from CostRecords; unknown-price models surface as "unpriced tokens", never silently estimated (BR-2)
- [ ] Per-phase attribution matches the session's phase at call time (AC-4 backend)
- [ ] Savings baseline is labeled as an estimate with its methodology string (OQ-6: same-token-volume repricing) in the report payload
- [ ] No credential or prompt content in any ledger row (BR-7)

## Technical Notes

Ledger rows store token counts and metadata ONLY — no prompt text (privacy +
BR-7). The savings methodology is deliberately simple and honest for MVP;
`report.rs` carries the methodology string so the CLI can display it —
never present the estimate as measured fact.

---
id: TASK-045
title: "Price by model, and name the models the meter could not price"
status: draft
parent: REQ-557
created: 2026-08-05
updated: 2026-08-05
dependencies: [TASK-043]
---

## Description

Re-orient price lookup around the model string, and give the existing unpriced
bucket the model identity it lacks so a user can read off which model needs a
price.

**Scope is smaller than the spec's BR-9 reads.** `report.rs` already carries an
`UnpricedTotals` bucket and already refuses to invent a cost — the "never a `$0`
record" half of BR-9 is satisfied today and needs no work. What is missing is
that `UnpricedTotals` is `{calls, input_tokens, output_tokens}` with no model
name, so the report can say *how much* went unpriced but not *what* to price.

## Files to Create/Modify

- `crates/tetond/src/cost/prices.rs` — price lookup keyed by `model`; retain
  `ModelPrice.provider_id` for the baseline label; **delete** the legacy
  provider-id→model resolution helper once TASK-044's migration wiring no longer
  needs it
- `crates/tetond/src/cost/report.rs` — `UnpricedTotals` gains an ordered set of
  the model names it could not price; the report rendering names them
- `crates/tetond/src/cost/ledger.rs` — thread the model through to the unpriced
  bucket if the aggregation path drops it today

## Acceptance Criteria

- [ ] Two providers declaring the same `model` are priced identically from one
      price entry, with no duplicate entry required (AC-7).
- [ ] A provider declaring a model absent from the price table produces a record
      in the unpriced bucket, not a `usd: 0` record — pinned by a test that would
      fail if a zero-cost record were emitted (AC-7).
- [ ] `teton cost` names every model in the unpriced bucket. A session that calls
      two unpriced models lists both by name; the rendering states what to do
      (add a price entry), not merely that something was unpriced (AC-7b).
- [ ] The existing `report.rs` unpriced-bucket tests pass **unmodified** —
      this task adds identity to the bucket, it does not change the accounting.
- [ ] The savings estimate and baseline label are unchanged; a test pins
      `baseline_model` still renders as `provider/model`.
- [ ] The legacy provider-id→model helper no longer exists in the workspace.

## Technical Notes

**Do not delete `ModelPrice.provider_id`.** The baseline label renders as
`provider/model` (`report.rs:80`, and its test asserts
`"anthropic/claude-opus-4"`), and the bundled table is authored with it. This
task changes which field the *lookup* keys on, not the record's shape. ADR-A
constrains the observable; the spec's BR-9 deliberately left the keying to this
phase.

**The bundled table needs no migration.** `PriceTable::bundled()` is embedded in
the binary and never read from disk (`runtime.rs:482`, `:549`) — there is no
user-owned price file to migrate.

**Deleting the legacy helper is this task's job, and it is load-bearing.**
TASK-043's migration and TASK-044's wiring construct a provider-id→model
resolver for the one-shot migration. Once migration is complete that path must
not survive: a live provider-id→model lookup is exactly the derivation ADR-A
deletes. LESSON-443's shape — a helper that outlives the condition it was written
for — is the risk. Removing it is an acceptance criterion above, not a cleanup
note.

**Ordered set, not `Vec`.** The unpriced model list is rendered to a user and
compared in tests; a `BTreeSet<String>` keeps output deterministic without a
sort at the render site.

---
id: TASK-115
title: "Parse completion_tokens_details.reasoning_tokens through TokenUsage, CostRecord, and the ledger"
status: pending
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

The attribution leg, independent of the effort chain. `teton cost` today cannot
say that 80% of a call was thinking, because neither adapter parses
`completion_tokens_details.reasoning_tokens` — verified absent tree-wide. This
task carries that count from the OpenAI-compatible usage chunk to a nullable
ledger column.

BR-10 is emphatic that this is an **attribution** change, not a totals change:
reasoning tokens are already inside `completion_tokens` / `output_tokens`, so
`output_tokens` must stay byte-identical for every workload.

## Files to Create/Modify

- `crates/teton-providers/src/lib.rs` — `TokenUsage.reasoning_tokens: Option<u64>` (:97)
- `crates/teton-providers/src/openai_compat.rs` — parse the field in the usage
  chunk handler (:245)
- `crates/teton-protocol/src/events.rs` — `CostRecord.reasoning_tokens: Option<u64>` (:478)
- `crates/tetond/src/cost/ledger.rs` — `SCHEMA` column + `ADDITIVE_COLUMNS` entry
  (:130) + the insert/read paths
- `crates/tetond/src/cost/mod.rs`, `crates/tetond/src/harness/completion.rs` — carry it through

## Acceptance Criteria

- [ ] `TokenUsage.reasoning_tokens: Option<u64>`. `TokenUsage` derives `Default`,
      so the field defaults to `None` — **`None` means unreported, never `0`**
      (BR-10). A test asserts the distinction survives serde.
- [ ] The OpenAI-compatible usage handler reads
      `usage.completion_tokens_details.reasoning_tokens` when present. Absent,
      null, or a non-integer → `None`, never `0`.
- [ ] **`output_tokens` is unchanged by this task.** A test parses a fixture usage
      chunk that carries `completion_tokens_details` and asserts `output_tokens`
      equals what the pre-REQ parser produced for the same bytes — the subset
      relationship BR-10 requires, proven rather than asserted in prose.
- [ ] A test asserts `reasoning_tokens <= output_tokens` whenever both are known,
      and that the two are never summed anywhere (grep the cost aggregation for
      any expression adding them).
- [ ] The Anthropic adapter is **not** changed: it reports no reasoning count, so
      its `reasoning_tokens` stays `None`. A test pins that, so a future reader
      does not mistake the absence for an oversight.
- [ ] `CostRecord.reasoning_tokens: Option<u64>` with
      `#[serde(skip_serializing_if = "Option::is_none", default)]` — the exact
      shape of `cached_tokens` (REQ-564), so no `PROTOCOL_VERSION` bump. Asserted
      against literal JSON in both directions.
- [ ] `cost_records` gains a nullable `reasoning_tokens INTEGER` column in
      `SCHEMA` **and** a matching `ADDITIVE_COLUMNS` entry — a column added to
      `SCHEMA` alone never reaches an existing `cost.db`, because
      `CREATE TABLE IF NOT EXISTS` is a no-op there.
- [ ] Opening a `cost.db` written by a pre-REQ build succeeds, adds the column,
      and reads historical rows back with `reasoning_tokens: None`. Rows are
      **never backfilled** — a row written before the column existed predates the
      concept, which is the truth about it, not a zero. The append-only triggers
      would reject a backfill anyway.
- [ ] A round-trip test: record a call with `Some(1234)`, read it back as
      `Some(1234)`; record one with `None`, read back `None`.

## Technical Notes

**`cached_tokens` (REQ-564) is the template for all of this** — same `Option<u64>`,
same `skip_serializing_if`, same `ADDITIVE_COLUMNS` entry, same
never-backfilled posture, and the same "component of, not addition to" comment
already on that field at `ledger.rs:225`. Follow it rather than inventing a
second convention.

**This task has no dependency on the effort chain and can land independently.**
It is listed second-to-last only because TASK-116 renders what it records.

**Do not add reasoning tokens to any total.** The cost meter's spend figure is
derived from `input_tokens` / `output_tokens`; adding reasoning tokens would
double-count and inflate every reported figure, which for a product whose
headline promise is cost control is worse than not reporting the split at all
(REQ-544 BR-2).

**The usage chunk is scanned with a carry buffer** (`CARRY_BYTES`, `ledger.rs`)
so a key split across chunk boundaries still matches. `completion_tokens_details.reasoning_tokens`
is a longer key path than anything currently scanned — check that
`CARRY_BYTES` (64) still comfortably exceeds the longest key plus its number, and
raise it with a comment if not.

---
id: TASK-063
title: "The compact duty — decide what to forget, ahead of the hard budget gate"
status: draft
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: [TASK-062]
---

## Description

Wire `Category::Compact` (scan tier). The duty decides which conversation blocks
to forget when context is under pressure. It runs at a **soft threshold**, ahead
of the existing unconditional `truncate_to_budget()` call (BR-4a, OQ-3 resolved).

**This is the highest-risk task in the REQ.** A bad compaction silently corrupts
every later turn, and unlike the other three duties its fallback is not "today's
behaviour at the same moment" — it is a different algorithm at a different time.
It warrants the most adversarial review.

## Files to Create/Modify

- `crates/tetond/src/harness/compact.rs` — **new**. Prompt builder, `COMPACT_OUTPUT_CONTRACT`, `COMPACT_OUTPUT_MAX_BYTES`, the soft-threshold constant, and the response parser.
- `crates/tetond/src/harness/context.rs` — add `compact_if_pressured()`, called immediately **before** the existing `ctx.truncate_to_budget()` at line ~618. Add `CompactionOutcome { dropped_blocks, degraded }`.
- `crates/tetond/src/harness/mod.rs` — declare the `compact` module.
- `crates/tetond/src/runtime.rs` — add `compact_route()` spelling `router.resolve(Category::Compact)` literally.
- `crates/tetond/src/call_sites.rs` — flip `Category::Compact` to `true`.

## Acceptance Criteria

- [ ] **The existing `ctx.truncate_to_budget()` call at `context.rs:618` is not modified, not wrapped, and not made conditional** (ADR-4). The duty runs ahead of it; it still runs unconditionally afterward.
- [ ] **AC-14**: with the duty stubbed three ways — never returns, returns garbage, entirely unrouted — the context is under budget after each. This proves the budget is enforced by `truncate_to_budget()` and not by the duty.
- [ ] **AC-7**: a forced failure leaves the context under budget with `degraded: true` on the `CompactionOutcome`. A test asserts the "keep everything" fallback is **not** taken — an over-budget context after a failed compaction is the failure this AC exists to catch.
- [ ] **BR-4**: never applies a compaction partially. A parse failure discards the whole response and degrades; it does not drop the blocks it managed to parse. A half-applied compaction is the worst outcome available — it corrupts the context *and* leaves the budget unmet.
- [ ] **BR-4**: an **over-budget** duty response (one that would leave the context above budget) is rejected and degrades, rather than being applied and then rescued by the hard gate. The hard gate is a backstop, not the plan. Equally, the fallback is never "keep everything" — that breaks the budget by a different route.
- [ ] `router.resolve(Category::Compact)` appears literally; the scan finds it. The literal is the BR-1 call-site tag — the category is named in source, never derived from prompt text or a tool name.
- [ ] Emits `route_decided` (AC-2).
- [ ] Egress scoped to the conversation blocks' own provenance (BR-7). A `local-only` source in the conversation refuses the remote compaction while the turn proceeds.
- [ ] Bounded by `COMPACT_OUTPUT_MAX_BYTES`, test reads the constant (BR-8, AC-11). This is the loosest ceiling of the five — a compaction is a conversation.
- [ ] `ScriptedFileEngine` arm + no-block-consumed test (AC-12, BR-10) + contract-verbatim test.
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Technical Notes

**Before assuming green, check event-shape assertions elsewhere** (ADR-8's
TASK-062 amendment). Wiring a duty that fires changes what every "a
`route_decided` naming X means Y" assertion in the suite means. `title` broke two
such assertions because it fires on every session's first turn; `compact` fires
under context pressure, so any fixture that crosses the threshold is exposed.
When one breaks, split the claim so neither half is vacuous — do **not** relax it
to `>= 1` or delete the discriminating half.

**This duty is NOT tool-owned** — do not use the `Tool::refine` seam (ADR-10),
which is for `triage`/`shell` only. `compact` hangs off `ContextManager`.

`truncate_to_budget()` at `harness/context.rs:356-369`; called at `:618`. It
drops oldest blocks while over budget, preserves the system prompt and the most
recent block, and clamps an oversized single block in place.

**LESSON-483 applies directly.** The chain is duty → parse → `truncate_to_budget`.
Each link needs its own mutation, because a test that only mutates the outer link
leaves the inner fallback unverified. Record the mutations in a
`# What breaks which test` table comment at the module head, following the
convention at `crates/teton/src/loading.rs:30-43` — mutations **applied and
observed failing**, not reasoned about (LESSON-441).

**The soft threshold is a constant, not a magic number inline.** AC-14's tests
read it. Pick a value with headroom — the point of OQ-3's resolution is that
compaction runs *before* the emergency, so a threshold within a rounding error of
100% would defeat the whole decision.

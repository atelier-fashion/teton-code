# REQ-590 — Architecture

## Approach

Three edits carry the whole REQ. Everything else is tests and the consequences of these three.

1. **Give the generation reservation a constant home**, so `derive` can subtract it without
   constructing a `HarnessConfig`.
2. **Replace `derive`'s local short-circuit with a local *branch*** that carries
   `LOCAL_ENGINE_N_CTX` and that reservation into the shared arithmetic, keeping the
   `LocalEngine` bound.
3. **Re-point `COMPACT_OUTPUT_MAX_BYTES`** at the budget it repairs to, rather than at a constant
   that no longer equals it.

The spec's own "Provenance" section records what an adversarial pass broke in its first draft.
This document adds what the exploration pass got wrong — see ADR-8. **Both classes of error had
the same shape: reasoning about what a number *should* be instead of reading what it *is*.**

## Key decisions

### ADR-1 — The reservation becomes a constant, and `generation_reservation()` stops building a config

`HarnessConfig::default()` (`turn_loop.rs:493`) calls `derive(BudgetInputs::local())`.
`generation_reservation()` (`budget.rs:614`) calls `HarnessConfig::default()` to read one `u32`
off it. Today the cycle is open only because `derive`'s `is_local` arm returns before reading
anything and `BudgetInputs::local()` hardcodes `reservation: 0`. **The short-circuit this REQ
removes is the only thing holding that cycle open.**

**Decision:** hoist the literal `1_024` out of `HarnessConfig::default()`'s `gen_params` into
`LOCAL_GENERATION_RESERVATION` in `budget.rs`. `gen_params.max_tokens` reads it,
`BudgetInputs::local()` reads it, and `generation_reservation()` **returns it directly** instead
of constructing a `HarnessConfig`.

**Rationale.** The cycle then cannot exist — not because an implementer was careful, but because
`generation_reservation()` no longer touches `HarnessConfig` at all. A guard that depends on
someone remembering it is not a guard (LESSON-508). It is also strictly better independent of
this REQ: six call sites currently build an entire `HarnessConfig` to read one `u32`
(`router.rs:630`, `:2733`, `server.rs:11541`, `budget.rs:655`, `:3389`, `context.rs:1732`).

`budget.rs` is the right home: it already holds `LOCAL_BUDGET_TOKENS`, `LOCAL_BUDGET_BYTES` and
`LOCAL_DIGEST_THRESHOLD_*`, and it is the module the spec's one-home rule (LESSON-456) already
points at for this family.

### ADR-2 — `derive` keeps a local branch; it does not lose one

**Decision:** the `is_local` arm stays and gains a body. It supplies `window: LOCAL_ENGINE_N_CTX`
and `reservation: LOCAL_GENERATION_RESERVATION`, runs the shared `(window − reservation)`
arithmetic, and stamps `BudgetBound::LocalEngine`.

**Rationale.** Deleting the arm outright drops `HarnessConfig::default()`'s call into the
`window == 0` path, which stamps `DefaultUnknown` — and `bound_clause` then renders
*"— set `capabilities.max_context` for this provider"*, the exact remedy BR-6 forbids for a route
with no provider declaration. The existing pin (`turn_loop.rs:465-468`) compares only the
*numbers*, so it would stay green while the bound flipped. AC-2 asserts the bound explicitly for
this reason.

### ADR-3 — The window is `LOCAL_ENGINE_N_CTX`, and it is not feature-gated (verified)

D-1 reversed to the constant because llama.cpp does not clamp `n_ctx` downward. The remaining
question was whether an ungated `derive` may read a constant that lives in `runtime.rs` beside
the `llama`-gated loader.

**Verified, not assumed: it may.** `egress/redact.rs` has **zero** `cfg(feature)` gates, imports
`LOCAL_ENGINE_N_CTX` at line 101, and derives `REDACT_PROMPT_BUDGET_BYTES` from it at line 133 —
and the default build compiles and passes. The constant is a plain `pub(crate) const` at
`runtime.rs:11999`, outside any gated block.

**This is already LESSON-499's prescribed shape** — the *decision* (what to subtract, what ratio
to apply) sits on the CI-visible side; only the engine that consumes the window is gated. No
extraction work is needed.

### ADR-4 — The constants stay; only the tier's derivation moves

**Decision:** `LOCAL_BUDGET_TOKENS` (4,096) and `LOCAL_BUDGET_BYTES` (32,768) are **unchanged**.
They remain the pair a route with genuinely no window runs under: a remote provider declaring
`max_context = 0`, and the unresolvable route.

**Recorded because both exploration agents assumed the opposite** and derived a chain of
consequences from it (see ADR-8). The consequences that follow from the constants *not* moving
are the ones that matter:

| Derived constant | Definition | Moves? |
|---|---|---|
| `MIN_BUDGET_BYTES` | `LOCAL_BUDGET_BYTES / 2` = 16,384 | **No** — and 30,720 > 16,384, so the floor never bites locally |
| `MIN_BUDGET_TOKENS` | `MIN_BUDGET_BYTES / 8` = 2,048 | **No** |
| `LOCAL_DIGEST_THRESHOLD_*` | 1,500 / 12,000 | **No** — they are the fraction's *base*, not its product |
| `COMPACT_OUTPUT_MAX_BYTES` | `LOCAL_BUDGET_BYTES` = 32,768 | **No — and that is the bug.** See ADR-5 |
| `REDACT_PROMPT_BUDGET_BYTES`, `REDACT_CHUNK_MAX_BYTES`, `REDACT_SCANNABLE_CONTEXT_BYTES`, `COMPACT_PROMPT_BUDGET_BYTES` | all from `LOCAL_ENGINE_N_CTX` | **No** — this REQ does not touch that constant (Out of Scope) |

### ADR-5 — `COMPACT_OUTPUT_MAX_BYTES` follows the budget, not the constant

`compact.rs:134` defines it as `LOCAL_BUDGET_BYTES`. Its own doc states the invariant: *a repair
may not return more than the budget it is repairing to.* With the constant frozen at 32,768 and
the local budget at 30,720, the invariant breaks — and it breaks **silently**: a compaction whose
candidate lands in the 2,048-byte gap is rejected at `context.rs:1492` and the turn falls back to
deterministic oldest-first eviction, on the route that most needed the model's judgement.

**Decision:** re-point it at the local route's derived byte budget.

**Rationale — LESSON-491 verbatim:** *"when two budgets constrain one flow, write the chain down
once and derive each number from its neighbor; any two 'independent' numbers on one chain are a
bug waiting to happen."* The chain here is
`engine window → route budget → compaction output ceiling`, and the third link was pinned to the
first's old value.

### ADR-6 — Zero word slack is accepted, and must therefore be asserted

The derivation sets `words × 3/2 = usable` **exactly**: 10,240 × 3/2 = 15,360 = 16,384 − 1,024.
Today the local word half carries 2.5× headroom.

**LESSON-496 is about precisely this shape** — *"an ordering policy is only as meaningful as the
gap between the limit and the count; it silently becomes 'never' the moment `limit == count`,
and nothing in the code says so: the two numbers live in different places, were chosen for
different reasons, and their coincidence is invisible at both definition sites."* Its prescribed
habit is to **assert the headroom**.

**Decision:** D-3 accepts the zero gap, so the assertion is inverted rather than dropped — a test
pins that the gap **is** zero, deliberately, with the reasoning at the assertion. A future change
to either the ratio or the reservation then reddens a test that explains itself, instead of
silently restoring or worsening a margin nobody knew was at zero.

`budget.rs:205-212` measures Rust at 1.69 tokens/word against a 1.5 ratio, so content denser than
the ratio overruns at full budget. The byte guard covers dense-*and-heavy* content; AC-9 measures
the uncovered quadrant (token-dense, byte-light) with a real tokenizer, because an assertion
written in whitespace words is structurally blind to it.

### ADR-7 — The 4,097 test is inverted, not renumbered

`turn_loop.rs:3365-3367` asserts:

```rust
assert_eq!(refusal.origin, ContextRefusalOrigin::LocalEngine);
assert_eq!(refusal.assembled_tokens, 4_097);
assert_eq!(refusal.budget_tokens, 4_096);
```

That is the **exact field report** that motivated REQ-589 and this REQ — 4,097 words against
4,096 — currently pinned as a *passing refusal*.

**Decision:** it is not a number to update. AC-12 requires that turn to serve, so the test's
premise is deleted: it becomes an assertion that 4,097 words on the local tier is **not** refused
and raises no over-budget offer. A refusal case at the *new* boundary is added separately, so the
refusal path keeps a witness.

**Recorded because the exploration pass listed this line as a mechanical `4,096 → 10,240`
renumber**, which would have kept the refusal and left AC-12 unwitnessed while the suite went
green — this REQ's own version of the AC-8 failure LESSON-560 describes.

### ADR-8 — Corrections to the exploration pass

Both agents reasoned from what a number ought to be rather than reading what it is. Recorded so a
reader who finds their output first is not misled:

- **"The constants change."** They do not (ADR-4). A chain of consequences was derived from this:
  `MIN_BUDGET_BYTES → 15,360`, `COMPACT_OUTPUT_MAX_BYTES → 30,720` automatically,
  `REDACT_CHUNK_MAX_BYTES → ~26,000`. All false. Notably, the claim that
  `COMPACT_OUTPUT_MAX_BYTES` follows automatically inverts ADR-5: it does **not** follow, which
  is why it needs an explicit fix.
- **"`LOCAL_ENGINE_N_CTX` is inside a `cfg(feature = "llama")` block."** It is not (ADR-3). Had
  it been true, the whole approach would have needed rethinking.
- **"`assert_eq!(LOCAL_BUDGET_BYTES, 32_768)` will fail immediately."** It will not — the
  constant is unchanged.
- **`web.rs:2465` reported as a production consumer** dividing `context_budget_bytes` by
  `REDACT_ESCAPING_DIVISOR`. It is inside a **test**.
- **The redact clamp**: correctly reported unreachable for local routes, before and after.

## What this REQ must preserve

- **The default build stays green without the `llama` feature.** Every AC except AC-10/AC-11
  (which measure a real engine) must run in ordinary CI. A criterion that only runs under a
  feature CI does not build is not a criterion.
- **`max_context = 0` still yields `(4096, 32768)`** — REQ-586 AC-1, on a fixture that is not the
  local route, so it cannot pass by coincidence.
- **Every mutation-verified guard stays mutation-verified.** Moving a test does not preserve its
  bite; re-running the mutation does (LESSON-563).

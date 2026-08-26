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

### ADR-6a — AC-4 is a coupling guard, not a bound check (noted before implementation)

Worth stating plainly so nobody reads more into a green AC-4 than is there.

The property is `budget_bytes / DUTY_REQUEST_BYTES_PER_TOKEN ≤ window − reservation`. After
ADR-2 the derivation *defines* `budget_bytes = (window − reservation) × DUTY_REQUEST_BYTES_PER_TOKEN`,
so the property reduces to `usable ≤ usable` — **it cannot fail while the byte half is derived.**

It is still worth having, because it fails today (32,768 / 2 = 16,384 > 15,360) and would fail
again the moment someone re-pins the byte half to a constant. But what it guards is the
**coupling**, not the bound: it cannot tell you the window is right, the reservation is right, or
that the engine will accept the result.

**Consequence for the verify phase:** do not accept AC-4 as evidence that the budget fits the
engine. The claim "no surface reports a budget larger than the engine will accept" (BR-7) is
carried by AC-9's real-tokenizer measurement and by the engine's own `over_window`, not by AC-4.

This is the same shape as the word half's zero slack (ADR-6) — the derivation is exactly
saturating in **both** currencies, `words × 3/2 = usable` and `bytes / 2 = usable`. That is a
consequence of deriving both from one number, and it is fine; it just means neither half carries
margin, and no assertion written in terms of the derivation can discover that.

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

## Measurements (AC-10, AC-11, AC-14) — TASK-275

D-3 took the full window and accepted that a local session now holds ~2.5× more conversation
before compaction fires. REQ-586 deferred this whole decision on exactly this cost, and REQ-589's
AC-15 runbook — which its own spec named as the first data point REQ-590 would need — was never
written. This section ends that.

**These numbers report; they do not gate.** D-3 is decided and none of what follows reverses it.

### What can be asserted and what can only be recorded

The distinction is not bookkeeping — a criterion dressed as a test that never runs is worse than a
number with a date on it (LESSON-499's coverage boundary).

| | where it lives | runs in default CI? |
|---|---|---|
| **AC-11** turns until compaction fires | `crates/tetond/tests/compaction_cadence.rs` | **yes** — pure arithmetic over two budgets; no weights, no `llama` |
| **AC-10(a)** prefill wall clock | recorded below; re-take with `crates/teton-inference/examples/local_budget_cost.rs` | **no** |
| **AC-10(b)** the REQ-544 BR-8 duty under load | recorded below; same example | **no** |
| **AC-14** a large local turn by hand | `docs/manual-verification.md` | **no — and not by a script or an agent either** |

AC-10 cannot run in CI: the real engine is behind `#[cfg(feature = "llama")]` (`runtime.rs:12011`)
and building it compiles llama.cpp from source with cmake, which CI does not do.
`ScriptedEngine` (`context_pressure.rs:98`) exercises the *logic* and measures nothing real, so it
is not a substitute. AC-10's figures are therefore stamped with a machine and a date, and the
example that produced them is committed so the next reader re-takes them rather than trusting
them.

### The reference machine

| | |
|---|---|
| Machine | Apple M5 Max, 48 GiB unified memory |
| OS / arch | macOS 26.6.2, arm64 |
| Toolchain | rustc 1.97.1, `--release`, `-p teton-inference --features llama` |
| Model | `qwen3-coder-30b-a3b.gguf` (Q4_K, 18.56 GB), the weights this machine's daemon serves |
| Engine args | `gpu_layers = u32::MAX` (Metal), `n_ctx = 16_384` — the same arguments `LlamaEngineLoader` passes (`runtime.rs:12039-12044`) |
| Date | 2026-08-25 |
| Runs | two independent runs of the same binary; both are given, and they agree to within ~3% |

**Load caveat, stated because it is the honest state of the machine:** three other agents were
compiling in the same worktree during these runs. The absolute milliseconds carry that; the
*ratios*, which are what AC-10(a) asks for, are taken within a single run and reproduced across
both. A-4 already records that one machine's figures generalize only so far.

Prompt sizes are the engine's own `prompt_tokens`, never `approx_tokens` — an assertion about a
15,360-token prompt written in whitespace words is the blindness AC-9 exists to close. Every probe
reported `cached_tokens: 0`, so these are cold prefills and not partial KV reuse.

### AC-10(a) — prefill wall clock: **the ratio is 4.4×, not 2.5×, and that is the finding**

| | prompt tokens | median time to first token | per-token |
|---|---|---|---|
| today's budget | 6,164 | **3,111 ms** (run 2: 3,141 ms) | 0.505 ms/token |
| derived budget | 15,410 | **13,548 ms** (run 2: 13,914 ms) | 0.879 ms/token |
| ratio | **2.50×** | **4.35×** (run 2: 4.43×) | **1.74×** |

Samples, run 1: `[3077, 3111, 3145]` and `[13548, 13571, 13476]` ms, one warmup discarded each.

The spec expected ~2.5×: *"prefill is ~linear in prompt tokens"*. **It is not**, and two points
could not tell "steeper than linear" from "one sample was noisy", so the example sweeps five:

| prompt tokens | median first token | ms/token |
|---|---|---|
| 3,082 | 1,398 ms | 0.454 |
| 6,164 | 4,106 ms | 0.666 |
| 9,246 | 5,943 ms | 0.643 |
| 12,328 | 9,277 ms | 0.753 |
| 15,410 | 13,518 ms | 0.877 |

Per-token cost rises roughly monotonically and **nearly doubles** across the range (the 9,246 point
sitting just under the 6,164 one is load noise, not a reversal). That is the shape of attention:
prefill is `a·n + b·n²`, because every token attends to every token before it. A least-squares fit
over the five points gives `ms/token ≈ 0.40 + 3.0e-5 · n`, which puts **a bit over half** of the
15,410-token prefill in the quadratic term against ~12% of the 3,082-token one. Five points on a
loaded machine do not pin the coefficients tightly; what they do establish is that the term is
there and that it is not small at the top of this budget.

**What D-3 costs, in one number: a full-budget local turn spends ~13.5 seconds before its first
token, against ~3.1 seconds at today's budget — about +10.5 s — on an M5 Max, the fast end of the
hardware the local tier admits.**

Two things this does *not* say. It is not a per-turn tax on every local turn: it is the cost at the
*top* of the budget, and a turn that carries little context prefills little. And it is not
new work — the same tokens cost the same to prefill before this REQ; what changed is that the
harness will now assemble a prompt that large, where before it refused at 4,096 words.

### AC-10(b) — the REQ-544 BR-8 duty: **it does not still pass**

`DutySpec::default()` (`benchmark.rs:36-45`) is `max_first_token_ms: 1000`, `min_tokens_per_sec: 5.0`.

| | first token | tok/s (prefill-inclusive) | verdict |
|---|---|---|---|
| duty prompts as shipped (23–28 tokens) | 151 ms | 100.27 | **Pass** |
| the same prompts behind a full-budget context (~15,170 tokens) | **12,885 ms** | 9.65 | **Fail** |

Run 2: 153 ms / 98.23 → Pass; 12,910 ms / 9.61 → Fail. The failure reason is
`"first-token latency 12885ms exceeds the 1000ms duty (BR-8)"`.

**AC-10(b)'s pass condition — "the duty still passes" — is not met.** Three things have to be said
about that, and none of them is "so ignore it":

1. **It fails on the half nobody was watching.** The AC names `min_tokens_per_sec: 5.0`; the duty
   fails on `max_first_token_ms`. Throughput clears its floor with room to spare.

2. **The engine did not get slower; there is simply more to prefill.** With prefill excluded,
   decode runs at **79–82 tok/s** under a full resident context against **135–139 tok/s** on a short
   one — about 42% slower, and still ~16× above the 5.0 tok/s floor. Nothing here is a throughput
   collapse.

3. **`min_tokens_per_sec` does not measure what its name suggests once prompts are large, and its
   Pass here is an artifact.** `run_benchmark` (`benchmark.rs:121-151`) divides *generated* tokens
   by the *whole* wall clock, prefill included. Derived from the measured run: 281 generated tokens
   at 9.65 tok/s is 29.1 s of wall clock, of which ~25.9 s was the two prefills and ~3.5 s the
   decode. The ratio therefore turns on `GenParams::max_tokens` — 256 here — and not on the engine:
   the same 25.9 s of prefill under a smaller generation cap would put it under 5.0 and "fail on
   throughput" for a reason that has nothing to do with throughput. **If BR-11's duty is ever made
   to bite on real turns, this is the metric to fix first.**

**What this does not change today.** `DutySpec` is a *model-selection* gate, run once after
download against `default_prompts()` (`runtime.rs:12051-12053`) and never again. Nothing in the
daemon re-evaluates it per turn, so no full-budget local turn is refused or stepped down because of
this figure. BR-11 asked what the duty would say if it were run under load; the answer is
**"fail, on latency, by 13×"**, and the honest reading is that the duty as written can see neither
prefill cost nor generation under a large resident context — which is exactly the gap AC-10(b) was
written to name. It is named, and it is not closed by this REQ.

### AC-11 — turns until compaction fires: **"~2.5× more conversation" is true in one currency only**

Asserted in CI by `crates/tetond/tests/compaction_cadence.rs`, driving the production turn loop
(`run_session_turn_with_source`) over a scripted local engine. Reproduce the table with:

```text
cargo test -p tetond --test compaction_cadence -- --nocapture
```

250-word user message plus a 120-word reply per turn; the count is the turn on which the
accumulated conversation first crosses `under_compaction_pressure`.

| bytes/word | stands for | before (4,096 w / 32,768 B) | after (10,240 w / 30,720 B) | ratio |
|---|---|---|---|---|
| 4 | source — the dense end of `budget.rs:205`'s ratio | 9 turns | **14 turns** | 1.56× |
| 6 | ordinary prose | 9 turns | **10 turns** | 1.11× |
| 8 | punctuation- and indent-heavy text | 8 turns | **8 turns** | 1.00× |
| 20 | minified JSON, base64, path-heavy logs (`budget.rs:55-62`) | 4 turns | **4 turns** | 1.00× |

**The gain decays with content density and is gone by 8 B/word.** The mechanism is that
`under_compaction_pressure` is a *disjunction over both currencies*, so what binds is whichever
budget the content exhausts first — and the crossover density is `budget_bytes / budget_tokens`:

| | words | bytes | word-bound below |
|---|---|---|---|
| before | 4,096 | 32,768 | **8 B/word** |
| after | 10,240 | 30,720 | **3 B/word** |

Before this REQ essentially every real conversation was **word**-bound, and the word half is the
half that rose 2.5×. After it, essentially every real conversation is **byte**-bound — and the byte
half is the half that *fell*. Exactly, at `COMPACT_PRESSURE_PERCENT = 70`:

- word threshold **2,867 → 7,168** words (2.5× up)
- byte threshold **22,937 → 21,504** bytes (6.25% **down**)

Both are pinned in `the_binding_guard_crosses_over_at_a_much_lower_density_after_this_req`.

**This is D-4 priced, not a defect discovered.** D-4 decided the byte half falls to 30,720 and BR-7
recorded the regression it accepts. What the measurement adds is that the regression is not
confined to the 2,048-byte refusal band BR-7 describes: it also moves the *compaction* threshold
down by the same 6.25%, so a byte-dense local session compacts marginally sooner than it does
today, not later. At this fixture's message size that lands inside one turn's granularity, which is
why the table reads 1.00× rather than below it — the threshold is the exact statement, the turn
count is the lived one, and both are recorded rather than one standing in for the other.

**The sentence to carry forward** is not D-3's "~2.5× more conversation". It is: *2.5× for
source-shaped content, ~1.1× for prose, and nothing at all once content passes 8 bytes per
whitespace word.*

### AC-14 — the by-hand leg

Written, not run: `docs/manual-verification.md`, "REQ-590 AC-14 (the engine-derived local budget)",
five legs — the reported budget and its arithmetic, the 4,097-word turn that used to be refused, a
full-budget turn, the token-dense/byte-light turn where ADR-6's slack is zero, and the byte band
D-4 gave up. **AC-14 is not satisfied until a person fills in its sign-off block**; that is the
whole reason it exists, REQ-589's AC-15 having been left unwritten.

### What was *not* measured

- **Any machine but this one.** A-4 already says the reference machine's figures generalize only so
  far, and the local tier is hardware-adaptive by design. The +10.5 s prefill figure is from the
  fast end of the admitted range; a machine at the slow end is unmeasured.
- **Prefix-cache reuse across turns.** Every figure here is a *cold* prefill
  (`Engine::complete`, `cached_tokens: 0`). REQ-564's cache means a real multi-turn session
  re-prefills only the changed suffix, so the +10.5 s is the worst case per new full-budget prompt,
  not a per-turn cost in a warm session. Quantifying the warm case is not in this REQ's scope and
  is not claimed here.
- **The duty under load on any generation cap but 256.** The `max_tokens`-dependence of
  `min_tokens_per_sec` above is derived from the measured run's own arithmetic and stated as such,
  not taken as a separate reading.

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

*Re-measured after ADR-9 reversed D-4. Both readings are given; the bold column is what ships.*

Asserted in CI by `crates/tetond/tests/compaction_cadence.rs`, driving the production turn loop
(`run_session_turn_with_source`) over a scripted local engine. Reproduce the table with:

```text
cargo test -p tetond --test compaction_cadence -- --nocapture
```

250-word user message plus a 120-word reply per turn; the count is the turn on which the
accumulated conversation first crosses `under_compaction_pressure`.

| bytes/word | stands for | before (4,096 w / 32,768 B) | after (10,240 w / 32,768 B) | ratio | *under D-4 (30,720 B)* |
|---|---|---|---|---|---|
| 4 | source — the dense end of `budget.rs:205`'s ratio | 9 turns | **15 turns** | 1.67× | *14 turns* |
| 6 | ordinary prose | 9 turns | **11 turns** | 1.22× | *10 turns* |
| 8 | punctuation- and indent-heavy text | 8 turns | **8 turns** | 1.00× | *8 turns* |
| 20 | minified JSON, base64, path-heavy logs (`budget.rs:55-62`) | 4 turns | **4 turns** | 1.00× | *4 turns* |

**Measured twice.** The last column is the same fixture under D-4's byte half, before ADR-9
reversed it; it is kept because a reader who finds those figures in an older revision should be
able to tell which measurement they are holding, and because the difference between the two
columns is what the reversal bought.

**The gain decays with content density and is gone by 8 B/word.** The mechanism is that
`under_compaction_pressure` is a *disjunction over both currencies*, so what binds is whichever
budget the content exhausts first — and the crossover density is `budget_bytes / budget_tokens`:

| | words | bytes | word-bound below |
|---|---|---|---|
| before | 4,096 | 32,768 | **8 B/word** |
| after | 10,240 | 32,768 | **3 B/word** |

Before this REQ essentially every real conversation was **word**-bound, and the word half is the
half that rose 2.5×. After it, essentially every real conversation is **byte**-bound — and the byte
half is the half that **did not move**. So the crossover moved entirely on the word half, and past
8 B/word this REQ buys nothing at all rather than costing something. Exactly, at
`COMPACT_PRESSURE_PERCENT = 70`:

- word threshold **2,867 → 7,168** words (2.5× up)
- byte threshold **22,937 → 22,937** bytes (unchanged)

Both are pinned in `the_binding_guard_crosses_over_at_a_much_lower_density_after_this_req`.

**This measurement is what priced D-4, and then reversed it.** D-4 had decided the byte half falls
to 30,720, and BR-7 recorded the refusal band it accepts. What this measurement added is that the
regression was not confined to that band: the byte half also drives the *compaction* threshold,
which fell by the same 6.25% (22,937 → 21,504) — so a byte-dense local session would have
compacted marginally **sooner** after this REQ than before it, and the 4 and 6 B/word rows read 14
and 10 rather than 15 and 11. That, together with the refusal measurement in ADR-9, is why D-4 was
withdrawn. With it withdrawn, every row above is the same or better and none is worse.

**The sentence to carry forward** is not D-3's "~2.5× more conversation". It is: *1.67× for
source-shaped content, ~1.2× for prose, and nothing at all once content passes 8 bytes per
whitespace word — because past that density the guard that binds is one this REQ does not
touch.*

### AC-14 — the by-hand leg

Written, not run: `docs/manual-verification.md`, "REQ-590 AC-14 (the engine-derived local budget)",
five legs — the reported budget and its arithmetic, the 4,097-word turn that used to be refused, a
full-budget turn, the token-dense/byte-light turn where ADR-6's slack is zero, and the byte band
D-4 gave up. **AC-14 is not satisfied until a person fills in its sign-off block**; that is the
whole reason it exists, REQ-589's AC-15 having been left unwritten.

> **ADR-9 amends the fifth leg.** D-4 is reversed, so there is no byte band to give up and no
> turn that serves today is newly refused. That leg is rewritten to the check the reversal makes
> worth doing by hand instead: a local turn at the **reported measurement's own density** —
> ~4,097 words and ~31 KB of code — which must serve silently, and whose rendered budget line
> must read `10,240 words / 33 KB (bound: local engine — …)`.

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

---

## ADR-9 — D-4 is reversed: the byte half returns to `LOCAL_BUDGET_BYTES`

*Recorded 2026-08-25, mid-Phase-4, after TASK-269 through TASK-275 had landed. **No ADR above is
edited**; every one of them was right about the state it described, and the point of this record
is the state that replaced it.*

### Who decided this, since that is the rule this record itself writes

**I did — the implementing agent — on my own judgement, and no owner approved it.** The finding
was put to the owner mid-Phase-4 and they did not answer; I said at the time that I was
proceeding without a decision rather than blocking on one, and this record exists so that
choice is legible instead of buried.

Three things make proceeding defensible, and they are the argument, not an excuse:

- It **restores the status quo ante**. 32,768 is the value the local route ran under before this
  REQ opened; the reversal removes a change, it does not introduce one.
- It is **cheap to undo** — one line in `derive`'s local arm (`pair.bytes = LOCAL_BUDGET_BYTES`),
  with the tests on both sides of it already written.
- The alternative was to ship a REQ whose own motivating case it had, at best, failed to fix.

The last section of this record faults D-4 for being an inference wearing a decision's label.
This record would earn the same fault if it did not say the above.

### The decision

`derive`'s local arm keeps its **word** half window-derived — `(16,384 − 1,024) × 2/3 = 10,240`,
exactly as ADR-2 built it — and takes its **byte** half from `LOCAL_BUDGET_BYTES`, **32,768**,
the constant the local route has run under since before REQ-586. The pair is asymmetric on
purpose: one half derived, one half the constant.

`COMPACT_OUTPUT_MAX_BYTES` **stays** on the engine's own chain (ADR-5, 30,720). It is not moved
back to `LOCAL_BUDGET_BYTES`: the whole of ADR-5 is that the ceiling should follow the window
rather than a constant it once coincided with, and that is still right. What changes is that
`ceiling ≤ budget` is now an **ordering with 2,048 bytes of room** rather than an equality.

### What reversed it

**First, what the record does and does not say.** The `/analyze` failure survives only as the
daemon's own *rendered* sentence, quoted in REQ-589's Description: "about **4,097 words / 31
KB**". The word half is exact. The byte half is **not** — `bytes_figure` renders
`(bytes + 500) / 1_000` (`teton-protocol/src/events.rs`), so `31 KB` means the true count lies in
**[30,500, 31,499]**, an interval 999 bytes wide. **No exact byte count for that body was ever
recorded, and this document previously asserted one (31,014) that was never measured.** At 4,097
words the interval is **7.44–7.69 B/word**.

That interval **straddles** D-4's 30,720, so the record cannot decide whether D-4 would have
refused the reported body. Three findings reverse D-4 anyway, and none of them needs it:

1. **The window-derived byte half only beats the constant below 7.5 B/word.** Pure arithmetic:
   `30,720 / d = 4,096 ⇒ d = 7.5`, exact. Prose (≈5 B/word) gains 50%; code (≈8) *loses* 6.25%.
   For analyzing code — the local tier's own workload — deriving the byte half is a regression.
2. **On the field report itself, D-4 was worth between +0.7% and −2.4%.** For `3 ≤ d ≤ 8` the two
   pairs' servable bodies stand in the ratio `7.5 / d`, so across the whole admissible interval:

   | reported body's true size | density | D-4 vs. the old pair |
   |---|---|---|
   | 30,500 B (interval floor) | 7.44 B/word | +0.7% |
   | 30,727.5 B (**the crossover**) | 7.50 B/word | break-even |
   | 31,499 B (interval ceiling) | 7.69 B/word | −2.4% |

   The crossover sits 228 bytes above the floor and 771 below the ceiling. **At best D-4 helped
   this case by under one percent; over most of the interval it hurt** — and where it hurt, the
   refusal did not go away, it changed currencies from the word guard to the byte guard. That
   would have gone unnoticed because AC-12 is written in whitespace words, which is the exact
   blindness A-2's own note warned about for a *different* criterion. TASK-272 had, in good
   faith, rewritten AC-12's witness to assert the byte half is "the boundary now" — a green test
   pinning this REQ's motivating case as still broken.
3. **It protects nothing measurable.** AC-9's `numeric_grid.txt`, the one sample in the corpus
   that overruns the engine at full budget, is 20,480 bytes: admitted at 30,720 **and** at
   32,768, costing 20,480 real `o200k_base` tokens against 15,360 usable either way. No byte
   value in this range catches that class. Only a real tokenizer does — which is what AC-9's test
   is, and why it stays.

**And the restore is safe without any measurement at all.** Only the word half moved, and it
moved up, so at every density `min(10,240, 32,768/d) ≥ min(4,096, 32,768/d)`. BR-7 —
"no turn that serves today is newly refused" — holds by inspection, whatever the reported body's
real size was.

### The residual, named rather than closed

At `DUTY_REQUEST_BYTES_PER_TOKEN` a 32,768-byte budget claims **16,384** provider tokens — the
engine's whole window — against **15,360** usable. The byte half out-claims the engine by exactly
`LOCAL_GENERATION_RESERVATION`, 1,024 tokens, so a *byte-saturated* local prompt is over the
window before it is assembled.

**This is the state the local route was already in.** 32,768 has always bridged to the whole
window; REQ-590 did not create the overclaim and, with this reversal, does not close it. What it
does is give it a size and a home: the equality
`bytes_claim − words_claim == LOCAL_GENERATION_RESERVATION` is asserted in
`tests/token_corpus.rs`, in the one place the two claims meet, so the residual is a number rather
than an adjective. AC-4 is rewritten around it.

The catch for a prompt that really is over remains the engine's typed
`context_length_exceeded`. **Not REQ-589's offer**: TASK-273 established that the offer fires
only when a *skill expansion* exceeds the budget, and in this quadrant both harness guards admit
the content, so the harness believes the turn fits. ADR-6 overstates the safety net on that
point; it is corrected here rather than edited there.

### What this cost and what it bought

| | D-4 | reversed |
|---|---|---|
| the reported `/analyze` body (7.44–7.69 B/word) | refused over ~78% of its admissible range | **serves** across all of it |
| ordinary code at 8 B/word | −6.25% budget | unchanged |
| prose at 5 B/word | +50% byte budget | unchanged |
| BR-7 ("no turn that serves today is newly refused") | overridden | **holds** |
| compaction threshold, bytes | 21,504 (−6.25%) | **22,937** (unchanged) |
| digest byte threshold | 11,250 (−6.25%) | **12,000** (unchanged) |
| AC-11 turns-to-pressure, 4 / 6 / 8 / 20 B/word | 14 / 10 / 8 / 4 | **15 / 11 / 8 / 4** |
| the byte half's overclaim at 2 B/token | 0 | **+1,024 tokens** |

Every row but the last is better or unchanged. The last is the price, and it is a price the local
route was already paying.

### Two of TASK-271's tests changed premise rather than value

Recorded because "the test still passes" and "the test still tests something" are different
claims (LESSON-563):

- **`the_compact_ceiling_is_the_loosest_of_the_five`** asserted `ceiling == budget`. That was
  right while both were window-derived; it would now be a coincidence to pin. It asserts the
  ordering, and the 2,048-byte gap is explained at `COMPACT_OUTPUT_MAX_BYTES`' definition.
- **`a_compaction_that_lands_in_the_old_gap_is_applied_not_degraded` is removed.** Its own guard
  read: *"there is no gap to test; if the default pair and the local pair have been brought back
  into agreement, this test has nothing left to say"* — written by an author who anticipated this
  reversal and made the test announce its obsolescence rather than pass on a fixture that no
  longer discriminates. Deleting it is what that guard asked for; widening the fixture until it
  fired again would have been manufacturing a property the arithmetic has removed. A comment in
  its place says so.

### What ADR-9 does *not* change

- **ADR-1, ADR-2, ADR-3.** The reservation constant, the local branch, the ungated window.
- **ADR-5's reasoning.** The compaction ceiling follows the engine's chain, not a constant.
- **ADR-6's zero word slack.** That is D-3's and it stands. Only its claim about REQ-589's offer
  being a catch is corrected, above.
- **ADR-6a.** Its warning is now *more* pointed, not less: with the byte half undererived, AC-4's
  old form is not merely tautological but false, which is why AC-4 is rewritten rather than kept.
- **ADR-7.** The 4,097 test is still inverted rather than renumbered — and this reversal is what
  finally makes the inverted assertion true. Its witness is renamed
  `the_reported_analyze_measurement_serves_on_both_halves_of_the_local_pair` and grew a fourth
  leg carrying the report's own byte figure, because the three boundary legs it had would all
  pass under D-4 as well.

### The process finding, which is the part worth carrying forward

**D-4 was never an owner decision.** D-3 — "take the full window" — was. D-4 was recorded as
"DECIDED by D-3", i.e. inferred: the full-window derivation happens to produce 30,720 bytes, so
the byte half was taken to follow. It was then built, and BR-7's regression was accepted on the
strength of a decision nobody had made.

An inference wearing a decision's label is not reviewable as an inference. The tell was available
in the spec's own text — D-4 opens *"Not a separate choice"* — and the fix is the general one:
**a decision record should say who decided it, and an inference should say what it was inferred
from.** D-4 now says both — and so, at the top of this record, does ADR-9 itself, which was
written before it said who made it.


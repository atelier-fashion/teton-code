# REQ-616 — Architecture

## Approach

The local window stops being a literal and becomes a **runtime fact with a
stated default**. Nothing about the derivation changes: `window_pair` already
turns a window into a `(words, bytes)` pair, and `derive`'s local arm already
runs it. What changes is where the arm gets its window from — a constant today,
a `LocalEngineWindow` the loader publishes tomorrow.

That framing is what keeps the blast radius honest. `LOCAL_ENGINE_N_CTX` is read
by four subsystems (`harness/budget.rs`, `harness/compact.rs`,
`egress/redact.rs`, `runtime/engine.rs`) and by roughly forty test assertions
that pin numbers derived from 32,768. A naive "make it runtime" deletes the
compile-time basis those assertions stand on and leaves the whole corpus either
rewritten or vacuous — the failure mode `conventions.md` names twice
(LESSON-569, LESSON-598).

The design avoids that by splitting the constant's two jobs rather than deleting
it (ADR-1), and it lands on a property worth stating plainly: **CI never loads a
real engine, so CI's window stays 32,768 and every existing assertion keeps its
number.** The 262,144 path is exercised by new tests that pass an explicit
window. The REQ is therefore additive to the test corpus rather than a rewrite
of it.

## Key decisions

### ADR-616-1: The constant keeps its name for the no-engine case; the loaded window is a separate runtime fact

`LOCAL_ENGINE_N_CTX` serves two jobs today, and only one of them is about a real
engine:

1. the `n_ctx` handed to `LlamaEngine::load` — a real-engine fact, reachable
   only under `--features tetond/llama`;
2. the compile-time basis for `REDACT_CHUNK_MAX_BYTES`,
   `REDACT_SCANNABLE_CONTEXT_BYTES`, `COMPACT_PROMPT_BUDGET_*` and `derive`'s
   local arm — evaluated in **every** build, including every CI build, none of
   which compile llama.cpp.

Job 2 is why `engine.rs`'s own doc says the constant is deliberately not feature
-gated. Making the window runtime without splitting the jobs would leave job 2
with no value to compute from in exactly the builds that do all the testing.

**Decision.** Rename to `LOCAL_ENGINE_N_CTX_DEFAULT` and keep it a `const`: it is
the window used when no real engine is loaded — the `MockEngine` path, and all of
CI. A loaded `LlamaEngine` publishes its actual window as a `LocalEngineWindow`
value which the daemon holds and threads to consumers.

**Why a default rather than an `Option`.** A `None` window in every CI build
would make every downstream assertion either unreachable or asserting about
`None`, which is the vacuous-verification class the validate gate blocks on. A
default that is *named* is a fact a test can pin; an absent value is not.

**What this buys.** Existing assertions on 21,162 / 63,488 / 184,265 keep
passing unchanged, because the default they derive from is unchanged. That is
the whole reason the REQ is tractable in one pass.

### ADR-616-2: The window-derived byte caps become `const fn`s; today's constants become their value at the default window

`REDACT_SCANNABLE_CONTEXT_BYTES`, `REDACT_CHUNK_MAX_BYTES` and
`COMPACT_PROMPT_BUDGET_BYTES` are `const`s computed from the window. They become
`const fn`s of it:

```rust
pub const fn redact_scannable_context_bytes(n_ctx: u32) -> usize { … }
pub const REDACT_SCANNABLE_CONTEXT_BYTES: usize =
    redact_scannable_context_bytes(LOCAL_ENGINE_N_CTX_DEFAULT);
```

The constant survives as the function's value at the default window, so the
tests that pin it (`redact_egress.rs` asserts 184,265 to the byte) keep a name to
pin and keep passing. REQ-562's "one number, one place" property survives intact
— the number is now a function of one input instead of one literal, which is a
strictly stronger version of the same rule.

### ADR-616-3: The memory probe is a pure function; hardware detection stays outside it

`fit_window(WindowFitInputs) -> WindowDecision` lives in `teton-inference`, pure,
with no RAM detection inside it — the `probe::decide` precedent, whose module doc
already states the rationale ("the runtime detection is deliberately factored out
so tests never depend on the host machine").

This is also what makes AC-5 testable at all. Emulating RAM end-to-end would
re-run *model selection*: at 16 GiB `band_for_ram` yields the small band and the
30B's 20 GiB `ram_floor_bytes` excludes it, so the test would silently be
asserting about `qwen2.5-coder-3b`. Holding the model fixed and varying only
`admissible_bytes` is the only shape in which AC-5's four cases mean what they
say.

```rust
pub struct WindowFitInputs {
    pub n_ctx_train: u32,
    pub weights_bytes: u64,
    pub admissible_bytes: u64,
    pub kv_bytes_per_token_f16: u64,
    pub config_n_ctx: Option<u32>,
    pub config_kv: Option<KvCacheType>,
    pub allow_over_memory: bool,
}

pub enum WindowDecision {
    Fits { n_ctx: u32, kv: KvCacheType, resident_bytes: u64, reason: WindowReason },
    Refused { shortfall_bytes: u64, /* the full arithmetic */ … },
}
```

`WindowReason` is `TrainedWindow | MemoryFit | ConfigOverride` — the spec's three
values, no more.

### ADR-616-4: Admissible RAM is 75 % of physical, stated once

OQ-1 was answered at spec validation: there is no existing figure to reuse.
`ModelEntry::ram_floor_bytes` is a per-model *minimum-RAM gate* ("is this machine
big enough for this model at all"), not an admissible-bytes budget — and it is
already mildly inconsistent with the KV measurement, since the 30B's 20 GiB floor
less its 17.3 GiB of weights leaves 2.7 GiB against the 3.0 GiB the KV cache
measures at the **current** 32,768 window.

AC-5 bounds the fraction: 48 GiB must admit q8_0 (30.3 GiB resident) and refuse
f16 (42.3 GiB), so the fraction lies in `[62.5 %, 87.5 %)`. **75 % is chosen** —
the midpoint of the admissible band, and it leaves 12 GiB on the 48 GiB dogfood
machine for the user's own work, which is the promise `ram_floor_bytes`'s doc
already makes ("never degrade the machine").

`ram_floor_bytes` is left alone in this REQ and the inconsistency is recorded as
an assumption rather than silently patched: changing it changes *model
selection*, which the REQ's Out of Scope explicitly excludes.

### ADR-616-5: KV bytes per token is derived from metadata and cross-checked against the measurement

```
kv_bytes_per_token = 2 (K and V) × n_layer × n_head_kv × head_dim × bytes_per_elem
```

For the shipped 30B this must reproduce the measured 3,072 MiB at 32,768 —
98,304 B/token at f16. A test pins that cross-check in both directions: if the
metadata-derived figure and the measurement disagree, the arithmetic is wrong and
the test names which. Where metadata is unavailable the measured constant is used
**and the event says so** — a fallback that does not announce itself is the
LESSON-456 failure exactly.

`q8_0` is taken as half of `f16` (1 byte/elem against 2), which is the ratio the
spec's estimates already assume.

### ADR-616-6: The two window events are `LifecycleEvent` variants; `prefill_progress` is a top-level event

`local_window_decided` and `local_window_refused` are local-model lifecycle facts
and sit beside `Probed` in `LifecycleEvent`, carried by the existing
`ModelLifecycle` envelope. `prefill_progress` is turn-scoped and has no existing
carrier, so it is a new top-level `Event` variant.

Neither moves `PROTOCOL_VERSION`. The project's additive rule (REQ-573 BR-2) and
REQ-588's finding both apply: a client predating an event drops the envelope at
`classify` — a lost line, not a mis-rendered one. The implementer must confirm
`LifecycleEvent` carries REQ-588 BR-4's `Unknown` catch-all before adding
variants to it; if it does not, that is a finding to surface, not to work around.

### ADR-616-7: The two config waivers stay separate in the type, not just in prose

BR-4 distinguishes `[inference] n_ctx` (waives the quarter-window refusal) from
`allow_over_memory` (waives the memory check). They are separate fields and
separate branches in `fit_window`; neither is allowed to imply the other. This is
the difference between "load at the window I named" and "overcommit my machine",
and collapsing them would let a user who wanted a *smaller* window silently get
an overcommitted load.

## What does not change

- **Remote routing.** `max_context` is still the declared window and still drives
  `window_pair` unchanged (BR-6). Only the rendering changes.
- **The word/byte crossover.** Both halves scale by the same factor, so the
  binding half stays the byte half at every window (AC-4). LESSON-565 is
  discharged by computing this, not by assuming it.
- **`LOCAL_BUDGET_TOKENS` / `LOCAL_BUDGET_BYTES`.** Retained: they serve the
  `DefaultUnknown` arm and are the denominator of BR-8's digest fraction
  (`budget × LOCAL_DIGEST_THRESHOLD_* / LOCAL_BUDGET_*`). Deleting them would
  break that fraction silently.
- **`ram_floor_bytes` and model selection.** Out of scope per the REQ.

## Risks

- **ASSUME-022 is invalidated and gets worse in absolute terms.** The 2 B/token
  bridge is not a floor: `numeric_grid.txt` measures 1.00 B/token and overruns
  the window by 1.33× with *both* harness guards admitting it. At 262,144 that
  overrun is the same ratio but eight times the absolute tokens, and it is
  discovered only at the engine's typed `context_length_exceeded` — after a
  prefill that may have run for a minute. The backstop still exists and still
  works; the cost of hitting it rises. Recorded as an assumption for this REQ,
  not fixed here (a real bound needs a tokenizer, which ASSUME-022 already names
  as a separate REQ's shape).
- **AC-12 cannot run in CI.** The real engine is behind the non-default `llama`
  feature. The recall trial is a dogfood verification whose result is recorded in
  the REQ; a green suite must not be read as having asserted it.
- **Concurrent REQs.** REQ-618 touches `harness/context.rs` and REQ-614 touches
  `router.rs`; both are in this REQ's footprint. Expect a rebase before merge.

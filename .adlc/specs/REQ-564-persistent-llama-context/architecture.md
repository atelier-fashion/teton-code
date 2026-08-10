# REQ-564 — Architecture: persistent llama context (prefix-cached KV)

## Approach

Today `LlamaEngine::complete` builds a whole `LlamaContext` per call
(`crates/teton-inference/src/engine.rs:511-517`), prefills every token from
position zero, decodes, and drops the context. This REQ keeps **one** context
alive per loaded engine and, when a turn's token stream agrees with the
resident prefix, truncates the KV to the agreement point and prefills only the
suffix.

The design splits into two halves that are deliberately kept apart:

- **The decision** — "may this turn reuse the resident prefix, and how much?" —
  is a pure function over token ids in a new `prefix_cache` module. It is NOT
  behind the `llama` cargo feature, so it compiles and is unit-tested in every
  default/CI build.
- **The mechanism** — keeping a `LlamaContext` alive, truncating its KV,
  prefilling a suffix — lives in the feature-gated `llama` module and is
  exercised only by `--features tetond/llama` runs and by dogfooding.

That split is what makes this REQ testable at all. The `llama` feature is
non-default and CI never compiles it, so a design that put the reuse policy
inside the FFI module would ship its most subtle logic (BR-2's divergence rule,
BR-8's miss taxonomy) with zero automated coverage.

## Verification of the spec's load-bearing assumption

The spec required this to be checked before task breakdown. **Verified — no
binding upgrade is needed.** `llama-cpp-2` 0.1.151 exposes, on `LlamaContext`
(`src/context/kv_cache.rs`):

| Operation | Binding call | Used for |
|---|---|---|
| Truncate KV past a position | `clear_kv_cache_seq(Some(0), Some(p0), None)` | rewinding to the agreement point |
| Full reset | `clear_kv_cache()` | cold re-prefill in a retained context |
| Resident high-water mark | `kv_cache_seq_pos_max(0)` | debug assertion / diagnostics |

`External Dependencies: none new` therefore holds.

## Key decisions

### D-1: The cached context lives on a model-owning thread, not inside the engine struct

**Decision**: `LlamaEngine` becomes a *handle* to one dedicated OS thread that
owns the `LlamaModel` and, optionally, one live `LlamaContext` borrowing it.
`Engine::complete*` sends a request over a channel and blocks for the reply;
`Drop` sends `Shutdown` and joins.

**Rationale** — two independent blockers make the obvious "just add a
`cache: Option<LlamaContext>` field" unsound:

1. **Self-reference.** `LlamaModel::new_context<'a>(&'a self, …) ->
   LlamaContext<'a>` ties the context's lifetime to a borrow of the model.
   Storing the context beside the model in one struct is a self-referential
   type — it needs either `unsafe` lifetime erasure or a crate like
   `ouroboros`, and the spec's "no new external dependencies" rules the latter
   out.
2. **`LlamaContext` is `!Send`.** It holds a raw `NonNull<llama_context>` and
   the binding declares no `unsafe impl Send` (contrast `LlamaModel`, which
   has both `Send` and `Sync` at `src/model.rs:127-129`). But `Engine: Send` is
   required, the daemon holds `Arc<Mutex<dyn Engine>>`, and successive turns
   run on **different** `spawn_blocking` threads. Holding the context in the
   engine would force an `unsafe impl Send` asserting that llama.cpp contexts
   (including their Metal command queues) have no thread affinity — a claim
   about a callee we cannot verify from its source. LESSON-453 is exactly this
   failure mode: PR #7 merged green on two unverified callee contracts.

On a thread that owns both, the borrow is an ordinary stack borrow and the
context never crosses a thread boundary. Both problems disappear and the
implementation contains **no `unsafe`**.

**Consequences**: model loading moves onto that thread, so `LlamaEngine::load`
waits for a load reply (externally identical — it already blocks for minutes on
the pool). Streaming keeps its per-token early-stop contract via a paired
control channel: the worker sends a piece, then blocks for the `bool`. That
channel is created per call and outlives neither side, so the "caller went
away" case that `completion.rs` handles on the *async* bridge is unchanged.
`Drop` must join, so a panicking worker must still send its reply — the reply
channel closing is itself the error signal.

**Alternative rejected**: `unsafe impl Send` + `Box`ed model + declaration-order
drop. Smaller diff, but it buys a soundness claim we cannot discharge, in the
subsystem where a wrong answer is silent memory corruption on a user's machine
with 17 GB of weights resident.

### D-2: Reuse is decided by a pure function; the agreement rule is `min(cached, new-1)`

`prefix_cache::PrefixCacheState::probe(session, tokens) -> CacheDecision` over
`&[i32]` token ids — no llama types, so it compiles feature-free.

The rule is **not** a naive `tokens.starts_with(cached)`. Let
`reuse = min(cached.len(), tokens.len() - 1)`. It is a **Hit** iff
`reuse > 0`, the session matches, and `tokens[..reuse] == cached[..reuse]`.

The `- 1` is load-bearing: sampling needs logits, and logits come only from a
token actually decoded in the final batch. A prompt that exactly equals the
cache (a retry) would otherwise reuse everything, decode an empty batch, and
sample from stale logits. Always re-prefilling the final token keeps the batch
non-empty on every path. It also makes a *shorter* agreeing prompt safe: the
KV is truncated to `reuse` and the surplus discarded, which is a rewind, not
the "partial reuse past a divergence point" BR-2 forbids.

Miss reasons (BR-8) are distinguished at the probe, never inferred later:
`Cold` (no cache), `SessionSwitch` (cache belongs to another session),
`Divergent` (same session, token disagreement), `Evicted` (a one-shot flag set
by `evict()` and consumed by the next probe, so an evicted cache does not
report itself as merely cold). An **error** is never expressed as a miss and a
miss is never expressed as an error — the probe cannot fail.

### D-3: The over-window guard stays exactly where it is, ahead of the probe

BR-7 / LESSON-491 / LESSON-444. The guard measures the **fully tokenized
prompt** against `n_ctx - max_tokens` and runs after `str_to_token` and before
any llama.cpp call — i.e. before the cache probe. Reuse changes how many tokens
are *decoded*, never how many must *fit*: the KV still has to hold the whole
prompt. So the hit path and the miss path are guarded by one site with one
expression, which is what LESSON-491 asks for.

The expression is extracted into a free function
`engine::over_window(prompt_tokens, n_ctx, max_tokens) -> Option<EngineError>`
(not feature-gated) so the test double enforces the *same* guard rather than a
copy that can drift.

### D-4: Duties stay cold, on the unchanged `complete`

OQ-1 resolved cold-per-duty. Rather than teach the duty path to opt out, the
trait gains a **new** method and the old one keeps its meaning:

```rust
fn complete_cached(&mut self, session: &str, prompt: &str, params: &GenParams,
                   on_token: &mut dyn FnMut(&str) -> bool)
    -> Result<Completion, EngineError>
{ self.complete(prompt, params, on_token) }   // default: cold
```

Only `LocalEngineSource::produce_turn` calls it. `duty.rs::perform` and
`classify.rs` keep calling `complete`, so BR-5 holds *structurally* — a duty
cannot evict the agent slot, because it never reaches it. Every existing
implementor (`MockEngine`, and the `ScriptedEngine` / `GatedEngine` /
capture doubles across `crates/tetond/tests/`) compiles unchanged.

**Consequence, stated plainly**: a duty served between two agent turns now
allocates its own context *while* the agent's cached context is resident — two
KV allocations (~1.5 GiB each at `n_ctx` 16384) instead of one. This is a real
new peak. It is bounded (exactly two, never per-session accumulation — BR-3),
it is transient, and BR-4's eviction seam is the compensating control. Sizing
duty contexts to their own much smaller budgets is the obvious follow-up and is
recorded as one; it is out of scope here because a second `n_ctx` currency is
precisely the two-numbers trap LESSON-446 names.

### D-5: One event variant with an outcome enum

Following the REQ-563 `WebLookup` precedent (one variant, every ending is an
outcome) rather than three near-identical variants:

```rust
Event::PrefixCache(PrefixCache { session_id, model, outcome })
enum PrefixCacheOutcome {
    Hit     { cached_tokens: u64, new_tokens: u64 },
    Miss    { reason: PrefixCacheMiss, processed_tokens: u64 },
    Evicted { reason: EvictionReason },
}
```

### D-6: Local turns get ledger rows — they have none today

**Finding.** BR-9 says local-call ledger records "gain" a cached-tokens count,
but there are no local-call records to gain one: `CostLedger::record_call` is
reached only through `Egress::with_cost_meter`, i.e. the remote choke point.
The local path is explicitly transport-free. So BR-9 requires *creating* local
rows, not extending them — scope the spec did not price.

The work is small because the shapes already fit: `record_call` takes an
arbitrary `provider_id` (the ledger's own tests already pass `"local"`), and a
local model is absent from the price table, so `price()` returns `None` and the
row is recorded **unpriced** — which is the truth about local inference, and
lands correctly on the report's priced/unpriced split rather than claiming
$0.00 of spend.

`cached_tokens` is added as a nullable `ADD COLUMN` through the existing
`ADDITIVE_COLUMNS` mechanism (`cost/ledger.rs:130`), per that constant's own
documented contract: append-only store, historical rows read back `None`
because they predate the concept. Remote rows keep `None` forever.

### D-7: Eviction seam — and an honest note about what it is wired to

`Engine::evict_prefix_cache(&mut self)` (default no-op) drops the resident
context and arms the `Evicted` miss reason. Engine unload/swap drops the cache
for free, since the cache dies with the thread.

**Finding.** BR-4 says "under the existing runtime memory-pressure adaptation".
`PressureController` is exported from `teton-inference` but **is not consumed
anywhere in `tetond`** — it has unit tests and no wiring. So there is no live
pressure signal to hang eviction off. This REQ therefore ships the seam and
proves it at the seam (AC-5 drives `evict_prefix_cache` directly); wiring
`PressureController` into the engine slot stays the separate REQ it already
was. Recording this rather than quietly implementing AC-5 against a mock
"pressure" that does not exist is the point.

## Test strategy, and what CI can actually prove

| AC | Covered by | Strength |
|---|---|---|
| AC-2, AC-3, AC-4, AC-8 | `prefix_cache` + `over_window` unit tests | full, default build |
| AC-1, AC-6, AC-9 | e2e over a scripted engine that implements `complete_cached` via the *same* `PrefixCacheState` | full, default build |
| AC-5 | direct `evict_prefix_cache` at the seam | full, default build |
| AC-7 | existing `crates/tetond/tests/nonblocking_inference.rs`, unchanged | full, default build |
| Real KV reuse through llama.cpp | `--features tetond/llama` + weights | **manual / dogfood only — CI does not compile this** |

The last row is the honest limit. CI can prove the policy, the plumbing, the
guard, and the accounting; it cannot prove that llama.cpp actually reuses the
KV, because CI never builds the FFI. The task list requires a dogfood
measurement (context create/destroy count over a multi-turn session) as the
evidence for that claim, not a green pipeline.

## Known limitations, found at verify

Recorded rather than quietly shipped. None is a correctness defect; each is a
place where the shipped behavior is narrower than a reader of the spec would
assume.

### L-1: a turn whose fabricated tail was cut will miss — and that is the common case

The KV holds every token the model **decoded**. The context holds what the
harness **kept**. Those differ exactly when BUG-147's `ReplyScanner` cuts a
fabricated continuation: the model ran on inventing tool results and future
turns, all of it decoded into the KV, and `context_cut` dropped it before it
reached context. The next turn's prompt therefore diverges from the resident
prefix at that point, and BR-2 mandates a full cold prefill.

So the cache serves well-behaved turns and misses fabricating ones — and
fabricating turns are precisely what the REQ's motivating measurement was full
of (an 11-generation loop on a weak local model). **The optimization is likely
to help least on the workload that motivated it.**

This is not a bug: it is BR-2 implemented as written ("Any divergence … falls
back to a full cold prefill from position zero; never partial reuse past a
divergence point"). The fix is a **spec** decision, not a code one. Reusing the
longest common prefix — reuse *up to* the divergence, never past it — would
recover this case and is arguably what "prefix cache" ought to mean, but it
contradicts the sentence above, so it is not something to slip in under an
implementation task. Recommend revisiting BR-2 before measuring the dogfood
result, since the measurement will otherwise look disappointing for a reason
that has nothing to do with the mechanism.

**RESOLVED (product decision, 2026-08-10): BR-2 amended to
longest-common-prefix reuse.** `PrefixCacheState::probe` walks the common
prefix instead of demanding total agreement over the compared head; the engine
side needed no change (it already rewinds to whatever offset `probe` returns
via `clear_kv_cache_seq`). The `-1` rule and its invariants are preserved:
a hit's `reuse` is always `>= 1` and `< tokens.len()`, so the final batch is
never empty. Correctness argument is unchanged from the rewind case already in
probe's contract: KV at positions `[0, reuse)` depends only on the agreeing
tokens, and the comparison is contiguous, so no disagreement can be skipped.
To keep the fabrication-cut rate measurable (BR-8's spirit), a hit whose reuse
was capped by a token disagreement rather than by prompt length carries
`divergent: true` on the `prefix_cache` hit event; `divergent` as a **miss**
reason now means divergence at token zero (or an unusably short prompt).

### L-2: session end does not evict

The spec's Events table lists "session end" as a `prefix_cache_evicted`
trigger. No business rule requires it and none of the ACs test it, and there is
no session-lifecycle hook to hang it off — REQ-565 is actively reworking daemon
and session lifetime, so adding one here would collide. The resident prefix
therefore outlives its session until another session takes the slot or the
engine unloads. No cross-session read is possible (the probe keys on session id
and reports `session_switch`), so this is a residency question, not a leak.

### L-3: a duty transiently doubles resident KV

Stated in D-4 and repeated here so it is not lost: with the agent's context
resident, a duty allocates its own, so peak KV during a duty is two contexts
(~1.5 GiB each at `n_ctx` 16384) rather than one. Bounded and transient, with
eviction as the compensating control. Sizing duty contexts to their own much
smaller budgets is the follow-up; it is deferred because a second `n_ctx`
currency is the two-numbers trap LESSON-446 names.

### L-4: eviction has no automatic trigger

D-7 already records this: `PressureController` is exported but unconsumed, so
`evict_prefix_cache` today has no production caller. The seam is built and
tested; wiring it is the separate REQ it already was.

## Proposed addition to `.adlc/context/architecture.md`

> **Policy is pure, mechanism is gated** — when a subsystem's interesting logic
> sits behind a non-default cargo feature that CI never compiles, the decision
> is extracted into a feature-free module over plain data and the gated module
> is left holding only FFI. Otherwise the subtlest code in the tree ships with
> the least coverage, and the test double and the real implementation enforce
> two different rules.

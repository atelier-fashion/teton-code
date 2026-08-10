---
id: REQ-564
title: "Persistent llama context: prefix-cached KV across agent turns"
status: approved
deployable: true
created: 2026-08-09
updated: 2026-08-10
component: "inference/local"
domain: "inference"
stack: ["rust", "llama.cpp", "gguf", "daemon"]
concerns: ["latency", "reliability"]
tags: ["kv-cache", "context-reuse", "prefill", "turn-loop", "session"]
---

## Description

Every local generation builds a brand-new `LlamaContext`: `Engine::complete`
(`crates/teton-inference/src/engine.rs:505-517`) allocates the context and its
KV cache (1,536 MiB at n_ctx 16,384), tokenizes the full prompt, prefills every
token from position zero, decodes, and frees the context. In an agent session,
consecutive turns share almost their entire prompt — system prompt plus the
conversation so far — so each turn re-processes up to 16k tokens that the
engine already processed one turn earlier.

Observed cost (2026-08-09 dogfooding, M5 Max, 17 GB model): 211 context
create/destroy cycles in the daemon log; a single user question drove an
11-generation agent loop and took over five minutes of wall time, most of it
redundant prefill. This is the single largest local-tier latency lever
(charter BR-8 latency duty).

Goal: within a session, a turn whose rendered prompt extends the previous
turn's prompt prefills **only the new suffix**, reusing the resident KV for the
shared prefix. Reuse is a pure optimization: any divergence falls back to a
full cold prefill.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| CachedContext | session_id | string | required; exactly one live CachedContext per loaded engine (single slot) |
| CachedContext | token_prefix | token id sequence | required; the tokens whose KV is resident, in order |
| CachedContext | n_ctx | number | equals the engine's context window; never grows |
| CachedContext | last_used | timestamp | required; for future LRU/diagnostics |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| prefix_cache_hit | new turn's token stream extends the cached prefix | session_id, cached_tokens (count reused), new_tokens (count prefilled) |
| prefix_cache_miss | no cache, divergent prompt, different session, or evicted | session_id, reason (cold \| divergent \| session_switch \| evicted), processed_tokens |
| prefix_cache_evicted | memory pressure, engine unload/swap, or session end | session_id, reason |

## Business Rules

- [ ] BR-1: **Correctness is observable-identical.** With identical prompt,
  params, and sampling seed, generation output with the cache enabled is
  byte-identical to a cold fresh-context generation. Prefix reuse must never be
  observable in output, only in latency.
- [ ] BR-2: **Reuse only on exact token-prefix extension.** A turn reuses
  cached KV only when its tokenized prompt starts with the cached
  `token_prefix` exactly (token-level comparison, not string-level). Any
  divergence — context compaction/truncation rewrote history, template change,
  session switch — falls back to a full cold prefill from position zero; never
  partial reuse past a divergence point. (informed by LESSON-447)
- [ ] BR-3: **Bounded memory: one cache slot.** At most one persistent context
  exists per loaded engine (the most recently used session). A session switch
  rebuilds the cache; KV memory never exceeds the single already-reserved
  context envelope. No per-session KV accumulation.
- [ ] BR-4: **Eviction is safe and silent-degrading, loud-reporting.** Under
  the existing runtime memory-pressure adaptation, the cached context is
  dropped before the engine is; a dropped cache must never fail a turn — the
  next turn cold-prefills — and the eviction is reported via the
  `prefix_cache_evicted` event, never silently. (informed by LESSON-447)
- [ ] BR-5: **Duty calls do not evict the agent cache.** Non-agent-turn
  purposes (summarize, classify, redaction duties) must not destroy the active
  session's cached agent context. How they are served (own small context,
  cold path, or other) is an architecture decision; the constraint is that a
  duty call between two agent turns leaves the second turn's cache hit intact.
- [ ] BR-6: **Blocking-pool discipline is preserved.** All cache probing,
  prefill, and decode still run inside `spawn_blocking` on the owned engine
  handle; no tokio worker is ever parked on inference. The invariant pinned by
  `tests/nonblocking_inference.rs` continues to hold. (informed by LESSON-448)
- [ ] BR-7: **The over-window guard survives the fast path.** The typed
  refusal of over-budget prompts (which prevents llama.cpp's `GGML_ASSERT`
  abort) runs on the rendered, tokenized prompt on **both** the hit and miss
  paths — enforcement stays at the last transform before the FFI call.
  (informed by LESSON-444, LESSON-491)
- [ ] BR-8: **Misses tell the truth.** Every miss carries its actual reason
  (cold / divergent / session_switch / evicted); a divergence must never be
  reported as an error, and an error must never be masked as a miss.
  (informed by BUG-146, BUG-152)
- [ ] BR-9: **Cost attribution distinguishes reused from processed tokens.**
  The cost ledger's local-call records gain a cached-tokens count alongside
  processed tokens, so the cost meter and future perf work can compute hit
  rates from recorded data.

## Acceptance Criteria

- [ ] AC-1: In a scripted 5-turn agent session (e2e harness, scripted engine or
  real small model), turns 2–5 each emit `prefix_cache_hit` and prefill only
  the suffix: instrumented processed-token counts equal the per-turn delta,
  not the full prompt length.
- [ ] AC-2: A/B correctness test: a scripted multi-turn session with fixed
  seed produces byte-identical outputs with the cache enabled vs disabled
  (BR-1).
- [ ] AC-3: Divergence test: a turn following a context compaction/truncation
  emits `prefix_cache_miss` with reason `divergent` and produces correct
  output via full re-prefill (BR-2).
- [ ] AC-4: Interleaved-session test: two sessions alternating turns produce
  correct outputs for both; cache thrash is permitted, wrong output is not
  (BR-3).
- [ ] AC-5: Eviction test: with the cache populated, a simulated memory
  pressure signal drops it (`prefix_cache_evicted` emitted); the next turn
  succeeds cold (BR-4).
- [ ] AC-6: Duty-interleave test: an agent turn, then a summarize duty call,
  then a second agent turn — the second agent turn is a cache hit (BR-5).
- [ ] AC-7: `tests/nonblocking_inference.rs` passes unchanged: unrelated RPCs
  complete while a gated cached-path generation is in flight (BR-6).
- [ ] AC-8: Over-window test: a prompt exceeding the window is refused with
  the typed error on the hit path as well as the miss path, with no process
  abort (BR-7).
- [ ] AC-9: Ledger rows for local calls carry cached vs processed token
  counts, and the summed counts across AC-1's session match the
  instrumentation (BR-9).

## External Dependencies

- None new. Relies on the existing `llama-cpp-2` binding (currently 0.1.151)
  exposing enough KV-cache/sequence API to keep a context alive across
  `complete` calls and to truncate/reset KV state — see Assumptions.

## Assumptions

- `llama-cpp-2` 0.1.151 exposes the KV-cache sequence operations needed to
  retain a context and clear/rewind past a token offset. If it does not, a
  binding upgrade becomes an external dependency — verify at architecture
  time before task breakdown. (informed by LESSON-453 — verify the callee's
  contract, don't assume it)
- Prompt rendering is deterministic turn-over-turn absent new content (same
  template arm, same neutralization), so real sessions produce genuine prefix
  extensions. The REQ-554 render pipeline gives no reason to doubt this, but
  AC-1 measures it rather than trusting it.
- A single cache slot is sufficient for the dominant usage pattern (one
  interactive session at a time).
- The `parse_special = true` behavioral dependency documented at
  `engine.rs:519-527` (REQ-554) is unaffected: tokenization happens on the
  same rendered string on both paths.

## Open Questions

- [x] OQ-1 — RESOLVED (product decision, 2026-08-10): **cold per duty**.
  Duties pay fresh-context setup; BR-5 only protects the agent cache from
  eviction. Measure duty-call frequency post-ship; revisit if hot.
- [ ] OQ-2: Is single-slot the right MVP bound, or should the slot count be a
  config value from day one (default 1) to avoid re-plumbing when multi-slot
  LRU arrives? (Architect's latitude — not user-blocking.)

## Out of Scope

- Multi-slot / LRU per-session caches (single slot only; see OQ-2).
- Cross-restart KV persistence (llama.cpp session files on disk). The cache
  dies with the daemon — including under REQ-565's exit-on-last-client
  lifecycle; that interaction is accepted.
- Prompt caching for remote providers (Anthropic prompt caching is a separate
  concern with its own billing semantics).
- Speculative decoding, batching, or any other local-tier performance work.
- Changing n_ctx, model selection, or the hardware-adaptation policy.

## Retrieved Context

- LESSON-474 (lesson, score 7): Frame on output, content on input — sanitize at the parser choke point
- LESSON-456 (lesson, score 6): The daemon knew but the error didn't say — never discard error evidence
- BUG-146 (bug, score 6): Misleading turn failure during tier load
- LESSON-444 (lesson, score 6): FFI asserts abort — guard inputs first
- LESSON-445 (lesson, score 6): Stage, then commit after authority re-check
- LESSON-446 (lesson, score 6): Token budgets must share a currency
- LESSON-447 (lesson, score 6): Fallbacks must preserve the guarded invariant
- LESSON-448 (lesson, score 6): Test-double speed masks executor blocking
- LESSON-452 (lesson, score 6): Decoder lifetime must match stream lifetime
- LESSON-453 (lesson, score 6): Verify callee buffer contracts
- LESSON-495 (lesson, score 4): A grant is only as narrow as its key
- LESSON-496 (lesson, score 4): Last in the order can mean never
- LESSON-491 (lesson, score 4): Enforce budgets at the last transform
- BUG-152 (bug, score 4): Warming tier reported as a turn error
- LESSON-443 (lesson, score 4): Guard conditions that disable themselves

Additionally read in-conversation (below retrieval cutoff, taxonomy predates
current vocabulary): BUG-147 — the turn-loop/hallucination bug whose fix
introduced the `ReplyScanner` early-stop; its investigation surfaced the
211-cycle context churn this REQ eliminates.

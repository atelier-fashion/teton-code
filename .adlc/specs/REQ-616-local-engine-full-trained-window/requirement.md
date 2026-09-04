---
id: REQ-616
title: "The local engine serves its full trained window — 262,144 tokens on the local tier, remote providers at their declared window, and every budget half, scan bound and surface following the window it runs under"
status: draft
deployable: true
created: 2026-09-04
updated: 2026-09-04
component: "daemon/router"
domain: "inference"
stack: ["rust", "llama.cpp", "daemon", "llm-providers", "gguf"]
concerns: ["latency", "cost", "reliability", "routing", "developer-experience"]
tags: ["context-window", "n_ctx", "n_ctx_train", "budget", "kv-cache", "max_context", "context-budget", "262k", "kv-quantization", "prefill", "redact-bound", "digest-threshold", "memory-probe", "local-engine"]
---

## Description

The product owner's decision (2026-09-04): **the local tier serves the local
model's full trained window, 262,144 tokens; remote providers keep the window
the user declared for them** (Kimi at 1,000,000 today). No RoPE scaling
beyond the trained window is in scope.

Today the local window is a compile-time constant and one eighth of what the
model can do:

| Route | Window today | Budget the harness derives | Bound |
|---|---|---|---|
| local (`Qwen3-Coder-30B-A3B-Instruct`) | `LOCAL_ENGINE_N_CTX = 32,768`, a constant in `runtime/engine.rs` | 21,162 tokens / 63,488 bytes | `local_engine` |
| remote (`kimi-k3`, `max_context = 1,000,000`) | 1,000,000, declared by the user | 665,984 tokens / 1,997,952 bytes | `window` |

The daemon log says so on every load: `n_ctx_seq (32768) < n_ctx_train
(262144) -- the full capacity of the model will not be utilized`. The model's
GGUF metadata carries `n_ctx_train = 262144` and `n_ctx_orig_yarn = 262144`,
so up to that window the model runs as trained, with no scaling and no
quality trade.

The 2026-09-04 session (`sess-23aczryx…`, v0.1.30) shows what the 32K window
costs. Pinned to the local tier by REQ-614's defect, the model had 21,162
tokens for a system prompt, the tool docs, a 25 KB `/analyze` skill body with a
4 KB ethos include, and every tool result; the `compact` duty ran ten times in
four prompts and the model lost the user's ask (REQ-618 covers what compaction
must keep; this REQ removes most of the reason it ran). At 262,144 the same
session would have fit eight times over.

The cost of the larger window is memory, and it is stated rather than hidden:
the KV cache measured 3,072 MiB at 32,768 tokens, so at 262,144 it is ≈ 24 GiB
at f16 or ≈ 12 GiB at q8_0, plus ≈ 18 GiB of weights. The dogfood machine has
48 GiB. This REQ derives the window from the model's own metadata, sizes the
KV cache to fit the machine, and refuses loudly where it cannot. Nothing is
clamped in silence (REQ-586's rule, kept). Remote routes change only in how
they are *described*: the window is reported in the provider's tokens, beside
the derived word budget, so 1,000,000 no longer reads as 665,984 (LESSON-446).

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| LocalEngineWindow (replaces the `LOCAL_ENGINE_N_CTX` constant) | n_ctx | u32 | `min(n_ctx_train, fit)` where `n_ctx_train` is read from the GGUF metadata and `fit` is the largest multiple of 4,096 the memory probe admits; never above `n_ctx_train` (no scaling) |
| LocalEngineWindow | kv_cache_type | enum `f16` / `q8_0` | chosen by the probe: `f16` when it fits at `n_ctx_train`, else `q8_0`; recorded in `model-selection.toml` |
| LocalEngineWindow | resident_bytes_estimate | u64 | weights + KV cache at `n_ctx` + compute buffers, computed before load and compared with the probe's admissible RAM |
| `[inference]` config (new table) | n_ctx | u32? | an explicit window; accepted only when `≤ n_ctx_train`, refused with the trained figure otherwise |
| `[inference]` config | kv_cache_type | `f16` / `q8_0`? | overrides the probe's choice |
| `[inference]` config | allow_over_memory | bool | permits a load whose estimate exceeds admissible RAM; default `false` |
| ProviderCapabilities (existing) | max_context | u32 | unchanged; the declared window is the window |
| RouteBudget (existing) | budget_tokens / budget_bytes | usize | derived from the route's window on every route, local included; the byte half derives from the window (`tokens × 3`), not from a constant |
| RouteBudget | window_tokens | u32 (new) | the window itself, reported beside the derived pair |
| DigestThresholds (existing) | tokens / bytes | usize | the same fraction of the budget as today, so they scale with the window |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `local_window_decided` (new) | engine load | `n_ctx`, `n_ctx_train`, `kv_cache_type`, `resident_bytes_estimate`, `admissible_bytes`, `reason` (`trained_window` / `memory_fit` / `config_override`) |
| `local_window_refused` (new) | not even `q8_0` at the smallest admitted multiple fits, and no override is set | the same fields plus `shortfall_bytes` and the remedy (`[inference] n_ctx`, `allow_over_memory`, or a smaller model) |
| `route_decided` (existing) | every route | gains `window_tokens` |
| `context_pressure` (existing) | any truncation | unchanged; AC-7 asserts its absence on the reference workload |
| `prefill_progress` (new) | a local prefill over 32,768 new tokens | `tokens_done`, `tokens_total`, `tokens_per_second`, at most once per second |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| set `[inference] n_ctx`, `kv_cache_type`, `allow_over_memory` | the user, through `config/set` (presence-attested where the build has it, REQ-576) or `teton model window <n>` |
| declare a remote provider's `max_context` | the user (unchanged, REQ-586); the daemon never raises or lowers a declared window |
| pick the KV type | the daemon's probe, reported in `local_window_decided`; never silent |

## Business Rules

- [ ] BR-1: **The local window is the model's trained window.** `LOCAL_ENGINE_N_CTX` ceases to be a constant. The loader reads `n_ctx_train` from the GGUF metadata and loads at that value when memory allows; for the shipped model that is 262,144. The daemon log's "full capacity will not be utilized" line must not appear on a machine the probe admits. This supersedes REQ-590 BR-4 ("the local window is `LOCAL_ENGINE_N_CTX`, the value the daemon loads with") — the value the daemon loads with is now derived, and REQ-590's derivation reads it from `LocalEngineWindow`. The redact duty's own budget, which also reads the constant today, follows BR-7.
- [ ] BR-2: **Never above the trained window.** No RoPE or YaRN scaling is applied; `[inference] n_ctx` above `n_ctx_train` is refused at config time with the trained figure named. A future REQ may lift this; it is a product decision, not a default.
- [ ] BR-3: **The KV cache is sized to fit, and the choice is reported.** The probe computes the resident estimate at `f16`; if it exceeds admissible RAM it recomputes at `q8_0`; if that fits, `q8_0` is used and `local_window_decided.reason = memory_fit` names both figures. If neither fits at `n_ctx_train`, the probe steps the window down by multiples of 4,096 at `q8_0` until it fits, and the reason names the step.
- [ ] BR-4: **A shortfall is refused loudly, not shrunk silently.** A load that would land below 65,536 tokens (one quarter of the trained window) without an explicit `[inference] n_ctx` emits `local_window_refused` with the arithmetic (weights, KV bytes per 4,096 tokens at each type, admissible RAM, shortfall) and does not load; an unattended session fails the local tier closed with the same reason (REQ-586: an unknown window is stated; LESSON-456: the daemon knew, the message must say). `allow_over_memory = true` loads at the trained window regardless, and the reason says so.
- [ ] BR-5: **The word half and the byte half both follow the window, on every route.** `budget_tokens = (window − generation_reservation) × 2/3` and `budget_bytes = budget_tokens × 3` for local and remote alike. The local byte constant `LOCAL_BUDGET_BYTES` no longer binds: REQ-590 ADR-9 kept it because the 2 B/token bridge at 32,768 derived a byte half *below* the constant, and at 262,144 the derived half (522,240 bytes) is above it by eight times (informed by REQ-590, LESSON-565: the crossover is computed and pinned in AC-4).
- [ ] BR-6: **Remote routes are unchanged in size and changed in description.** A remote provider routes with its declared `max_context` exactly as today. Every surface that prints its budget prints the window first, in tokens, then the derived budget with its currency named: `window 1,000,000 tokens; budget 665,984 words (≈1,000,000 tokens at 3/2)` (LESSON-446).
- [ ] BR-7: **The redact scan follows the local window or says it cannot.** `REDACT_SCANNABLE_CONTEXT_BYTES` is derived from `LocalEngineWindow` rather than from the old constant, because the redact duty runs on the local engine; where a route's budget still exceeds what the scan covers, `bound = redact_scan` is reported with the exact figure, as today.
- [ ] BR-8: **Digest and compaction thresholds scale with the window.** `digest_threshold_*` stays the same fraction of the budget, so a tool result digested at 32K is digested at 262K only when proportionally as large.
- [ ] BR-9: **Prefix caching carries the window.** REQ-564's persistent llama context is sized to the new `n_ctx`; a prefill over 32,768 new tokens emits `prefill_progress` so a long first turn is visibly working; the 120 s duty deadline does not apply to an agent-turn prefill.
- [ ] BR-10: **The catalog states each model's trained window and the window it will be served at.** `teton model list` shows `trained 262,144 · served 262,144 (KV q8_0)` per entry on this machine, or the fitted figure with its reason.

## Acceptance Criteria

- [ ] AC-1: On a machine the probe admits, `route_decided` for a local route reports `window_tokens = 262,144`, `budget_tokens = 174,080`, `budget_bytes = 522,240`, `bound = local_engine`; the daemon log shows `n_ctx = 262144` and no "full capacity will not be utilized" line.
- [ ] AC-2: On `kimi-k3` with `max_context = 1,000,000`, `route_decided` reports `window_tokens = 1,000,000` and `budget_tokens = 665,984`; `/provider list`, `/policy show` and `/doctor` print the BR-6 sentence with both figures.
- [ ] AC-3: A 170,000-word prompt on the local route is served without `context_pressure`; a 270,000-token prompt is refused by `over_window` with the window named, not truncated.
- [ ] AC-4: A pinned test computes the crossover of the word and byte halves at 262,144 for the three reference contents (prose, code, base64) and asserts the byte half is never the binding half for prose or code.
- [ ] AC-5: With RAM emulated at 48 GiB the probe picks `q8_0` at 262,144 (f16 estimate ≈ 42 GiB exceeds the admissible share) and `local_window_decided.reason = memory_fit`; at 96 GiB it picks `f16`; at 16 GiB it emits `local_window_refused` with the arithmetic and does not load unattended; `[inference] n_ctx = 65536` at 16 GiB loads at 65,536 with `reason = config_override`.
- [ ] AC-6: `[inference] n_ctx = 300000` is refused at `config/set` naming `n_ctx_train = 262144`.
- [ ] AC-7: `context_pressure` is not emitted on the reference workload — the 2026-09-04 transcript's four prompts, skill bodies and tool results replayed against a stub engine sized at 262,144.
- [ ] AC-8: The digest threshold test from REQ-558 passes at 32K and 262K with the same fraction; a test pins the fraction.
- [ ] AC-9: A local prefill of 100,000 tokens emits `prefill_progress` at least once and the turn does not hit a deadline.
- [ ] AC-10: The `redact` egress suite passes with the scan bound derived from the window; a route whose budget exceeds the scan reports `bound = redact_scan` with the exact figure.
- [ ] AC-11: `model-selection.toml` records `kv_cache_type` after load; `teton model status` and `teton model list` print BR-10's line.
- [ ] AC-12: A recall trial on the shipped local model at 200,000 tokens of repository context (a fact planted at the 10 %, 50 % and 90 % marks) retrieves all three, three of three runs, at the KV type the probe chose; the result is recorded in the REQ's verification notes.

## External Dependencies

- Quantized KV cache (`q8_0`) support through the llama.cpp crate the engine binds; the crate version is pinned in `Cargo.lock` and bumped if needed.
- GGUF metadata read for `n_ctx_train` before context allocation (the loader already prints it).

## Assumptions

- The per-token KV cost scales linearly with `n_ctx` from the measured 3,072 MiB at 32,768 (48 KV layers); at 262,144 that is ≈ 24 GiB f16, ≈ 12 GiB q8_0.
- `q8_0` KV has no measurable effect on tool-call parsing or recall for this model; AC-12 is the check.
- Remote providers that accept 1M tokens bill per input token; the cost meter already attributes it, and REQ-588's spend cap remains the user's ceiling.

## Open Questions

- [ ] OQ-1: What share of physical RAM is "admissible" for the probe — the existing model-selection rule, or a new one that accounts for the KV cache? Recommended: reuse the selection rule's headroom figure so one number governs both decisions.
- [ ] OQ-2: A cold 262K-token prefill on Apple Silicon is on the order of a minute or two at this model's throughput. Is `prefill_progress` enough, or is a per-turn prefill ceiling wanted?

## Out of Scope

- Any window above the model's trained window (RoPE/YaRN scaling); a 1M local window is a separate REQ if wanted.
- Changing any remote provider's declared `max_context`.
- Changing which local model ships or the hardware-adaptive selection rules beyond recording the KV type.
- Automatic compaction policy (REQ-618).

## Retrieved Context

- REQ-590 (spec, score 14): Derive the local tier's context budget from the engine's real window
- REQ-586 (spec, score 10): A turn's context budget follows its route
- REQ-564 (spec, score 10): Persistent llama context: prefix-cached KV across agent turns
- REQ-567 (spec, score 9): Cross-prompt conversation carry in interactive sessions
- REQ-557 (spec, score 8): Provider model identity and an explicit default provider
- LESSON-498 (lesson, score 8): A !Send FFI handle bound to a borrow wants a thread, not a struct field
- LESSON-446 (lesson, score 8): Token budgets that meet at a boundary must share a currency
- LESSON-565 (lesson, score 7): Raising one limit in a conjunction buys nothing you have not computed the crossover for
- REQ-588 (spec, score 7): A spend cap, and an event vocabulary a future kind cannot break
- LESSON-532 (lesson, score 7): Presence in context is not instruction-following
- REQ-558 (spec, score 7): Purpose-oriented routing categories as the runtime dispatch key
- REQ-559 (spec, score 7): Global reasoning effort with per-provider clamping
- LESSON-500 (lesson, score 7): A cache keyed on the conversation must account for what the harness threw away
- LESSON-456 (lesson, score 7): A `_`-discarded error is a silent downgrade
- BUG-146 (bug, score 7): First prompt after install fails with a message blaming the local engine

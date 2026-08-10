---
id: TASK-082
title: "LlamaEngine: model-owning thread, resident context, suffix-only prefill"
status: complete
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-079, TASK-081]
---

## Description

The mechanism half (architecture D-1). Restructure `LlamaEngine` into a handle
onto one dedicated OS thread that owns the `LlamaModel` and at most one live
`LlamaContext`, and implement suffix-only prefill against the resident KV.

This is the largest task in the REQ and is entirely inside the `llama`
feature-gated module, which CI does not compile.

## Files to Create/Modify

- `crates/teton-inference/src/engine.rs` — rewrite the `llama` module's
  `LlamaEngine`: thread + request/reply protocol, resident context, KV
  truncation, suffix prefill, `Drop`

## Acceptance Criteria

- [ ] `LlamaEngine::load` spawns the owner thread, loads the model **on** that
      thread, and returns only after a load reply — same external blocking
      semantics as today
- [ ] The resident `LlamaContext` never crosses a thread boundary; the module
      contains **no `unsafe`** and no new dependency
- [ ] `complete_cached` probes via `PrefixCacheState`, and on a Hit calls
      `clear_kv_cache_seq(Some(0), Some(reuse), None)` then decodes only
      `tokens[reuse..]`, starting batch positions at `reuse`
- [ ] On a Miss the context's KV is fully cleared and the prompt prefills from
      position 0 — byte-identical generation to today (BR-1)
- [ ] `complete` (the cold method) still builds and drops its own context, so
      duties are unaffected (BR-5)
- [ ] The over-window guard calls `over_window` once, after tokenization and
      before the probe, so it covers hit and miss identically (BR-7)
- [ ] `evict_prefix_cache` drops the resident context and arms the `Evicted`
      reason
- [ ] `Drop for LlamaEngine` signals shutdown and joins; a panicked worker
      surfaces as `EngineError::Backend`, never a daemon crash or a hang
- [ ] Sampling still reads logits from the final decoded token on both paths
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean, and
      `cargo clippy -p teton-inference --features llama` clean
- [ ] `cargo build -p teton-inference --features llama` succeeds

## Technical Notes

Request protocol: `Load`, `Complete { rendered, params, cache_key: Option<String> }`,
`Evict`, `Shutdown`. Per-token streaming keeps the existing early-stop contract
with a paired control channel — worker sends the piece, then blocks for the
`bool`. Both channel ends are created per call, so neither outlives the other
and the existing "the async caller went away" handling in
`harness/completion.rs` is untouched.

Keep everything the current implementation earned:
- `PieceDecoder` lives for the whole stream (LESSON-452) — one decoder per
  generation, flushed at the end.
- `N_BATCH` chunking on prefill; a decode may never exceed `n_batch`
  (`GGML_ASSERT` aborts the process — LESSON-444). Suffix chunking must offset
  positions by `reuse`, and the `last` index that requests logits is the last
  token of the **suffix**, not of the full prompt.
- The `parse_special = true` behavioral-dependency comment at the tokenize call
  (REQ-554) must survive the restructure verbatim.

After a successful generation, `record` the resident prefix as prompt tokens
**plus** the tokens actually decoded during generation — that is what the KV
holds. Recording only the prompt would make the next probe compare against a
prefix shorter than the real KV and silently corrupt the reuse offset.

On any error mid-generation, drop the resident context rather than leaving it
in an unknown KV state; the next turn cold-prefills. A fallback must preserve
the invariant it guards (LESSON-447) — here the invariant is "the recorded
prefix describes the resident KV exactly".

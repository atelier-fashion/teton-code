---
id: TASK-079
title: "Pure prefix-cache policy module and the shared over-window guard"
status: draft
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: []
---

## Description

Create the feature-free decision half of the design (architecture D-2, D-3):
the pure prefix-cache policy over token ids, and the single over-window guard
expression both the real engine and the test double will call.

Nothing in this task touches llama.cpp or the `llama` feature. It compiles and
is fully tested in a default `cargo test` run.

## Files to Create/Modify

- `crates/teton-inference/src/prefix_cache.rs` — new: `PrefixCacheState`,
  `CacheDecision`, `MissReason`, `EvictionReason`, `probe`, `record`, `evict`
- `crates/teton-inference/src/engine.rs` — add the free fn `over_window`
- `crates/teton-inference/src/lib.rs` — `pub mod prefix_cache;` and re-exports

## Acceptance Criteria

- [ ] `PrefixCacheState::probe(session, tokens) -> CacheDecision` is pure, total,
      and cannot return an error
- [ ] Hit iff `reuse > 0` and session matches and `tokens[..reuse] == cached[..reuse]`,
      where `reuse = min(cached.len(), tokens.len().saturating_sub(1))`
- [ ] A prompt exactly equal to the cached prefix is a Hit that still leaves one
      token to prefill (the `-1` rule) — covered by a named test
- [ ] Miss carries `Cold` / `SessionSwitch` / `Divergent` / `Evicted` distinctly (BR-8)
- [ ] `evict()` arms a one-shot flag so the next probe reports `Evicted`, not `Cold`;
      the flag is consumed (a second probe reports `Cold`)
- [ ] `over_window(prompt_tokens, n_ctx, max_tokens)` reproduces today's guard
      expression exactly, with a test asserting the boundary (`==` budget passes,
      `budget + 1` refuses) and the message text unchanged
- [ ] Divergence at position 0 is a Miss, not a zero-length Hit
- [ ] `cargo test -p teton-inference` passes with no `llama` feature

## Technical Notes

Token ids are `i32` here, not `LlamaToken`, so the module has no llama
dependency; the engine converts at its boundary.

`probe` must not mutate. `record(session, tokens)` installs the new resident
prefix after a successful generation — note that the resident prefix after a
turn is the **prompt tokens only**, not prompt + generated tokens, unless the
generated tokens are also decoded into the same context (they are — the decode
loop appends them). Decide explicitly and document: the resident prefix is
every token whose KV is actually in the context at end of turn, which is
prompt + emitted tokens. Getting this wrong makes the next turn's probe compare
against a prefix that does not match the KV, which is a correctness bug, not a
performance one.

BR-2 forbids partial reuse past a divergence point — the rule above never
reuses past the first disagreement because it compares a contiguous head only.

Do not widen the rule to string-level comparison (LESSON-447: a fallback must
preserve the guarded invariant; token-level is the invariant here).

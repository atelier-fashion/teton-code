---
id: TASK-081
title: "Engine trait: complete_cached + evict_prefix_cache seam"
status: complete
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-079]
---

## Description

Add the trait seam that lets the agent-turn path ask for a cached completion
while every duty path keeps the cold one (architecture D-4). Both new methods
are defaulted so no existing implementor changes.

## Files to Create/Modify

- `crates/teton-inference/src/engine.rs` — `Engine::complete_cached`,
  `Engine::evict_prefix_cache`; `Completion` gains `cached_tokens` and
  `cache_miss`
- `crates/teton-inference/src/lib.rs` — re-export any new public types

## Acceptance Criteria

- [ ] `complete_cached(&mut self, session, prompt, params, on_token)` defaults to
      delegating to `complete` (cold), so `MockEngine` and every test double
      compile unchanged
- [ ] `evict_prefix_cache(&mut self)` defaults to a no-op
- [ ] `Engine` remains object-safe — `Arc<Mutex<dyn Engine>>` still builds
- [ ] `Completion` gains `cached_tokens: u32` (0 on every cold path) and
      `cache_miss: Option<MissReason>` (`None` on a hit)
- [ ] Every `Completion { .. }` literal in the workspace is updated (MockEngine,
      the `crates/tetond/tests/` doubles, benchmark)
- [ ] Doc comments state that `processed_tokens` is derived
      (`prompt_tokens - cached_tokens`) and is not a stored field
- [ ] `cargo test --workspace` passes

## Technical Notes

`complete` keeps `&self` — do not change it. `complete_cached` takes `&mut self`
because a cached completion mutates cache state; `MutexGuard` derefs to `&mut`,
so the call sites become `let mut guard = engine.lock()…`.

`processed_tokens` is deliberately derived rather than stored: two stored
counts that must sum to a third is a drift surface (LESSON-446).

Do NOT add a `session` field to `complete`'s signature — the whole point of the
two-method split is that a duty cannot accidentally acquire a cache key.

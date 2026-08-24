---
id: TASK-239
title: "Produce the typed context outcome on the local engine"
status: draft
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

ADR-3 / D-11. `HarnessError::ContextLengthExceeded` is constructed only on the remote path (`completion.rs:535`, `:1259`). The local engine's window refusal becomes `HarnessError::Engine` → `INTERNAL_ERROR` (`runtime.rs:4057`). BR-3, BR-12 and BR-14.1 all name the typed outcome as their backstop on the local tier — the route the reported `/analyze` failure ran on. Build it. **Head of the DAG.**

## Files to Create/Modify

- `crates/tetond/src/harness/completion.rs` — `LocalEngineSource::produce_turn` (~285) yields the typed outcome on a window refusal
- `crates/tetond/src/runtime.rs` — the `HarnessError::Engine` arm (~4057) must no longer swallow it as INTERNAL_ERROR
- `crates/tetond/src/harness/turn_loop.rs` — `ContextLengthExceeded` (~179) carries a provider today; admit a local origin

## Acceptance Criteria

- [ ] A local-engine turn whose rendered prompt exceeds the engine window surfaces `error_code::CONTEXT_LENGTH_EXCEEDED`, not INTERNAL_ERROR
- [ ] Driven through `WindowedEngine` (`completion.rs:1865`), which already returns a backend error above a configured byte threshold
- [ ] The remote path's behaviour is byte-identical to today — a paired test pins it
- [ ] No caller distinguishes local from remote by string-matching an engine sentence (LESSON-528)

## Technical Notes

`WindowedEngine` is the instrument; `an_over_window_rendered_prompt_is_refused_with_the_typed_error` (completion.rs:1895) is the existing shape to extend. Do NOT reuse the remote `ProviderError` conversion — the local origin has no provider.

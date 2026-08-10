---
id: TASK-083
title: "Wire the session key through LocalEngineSource and emit prefix_cache events"
status: complete
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-080, TASK-081]
---

## Description

Give the local agent-turn path a cache key and make its outcome observable.
`LocalEngineSource` currently has no session id (unlike `RemoteProviderSource`,
which does); it needs one to key the cache and to attribute the event.

## Files to Create/Modify

- `crates/tetond/src/harness/completion.rs` — `LocalEngineSource` carries a
  `SessionId`; `produce_turn` calls `complete_cached` and reports the outcome
- `crates/tetond/src/harness/turn_loop.rs` — pass the session id at
  construction; expose the bus/session from `SessionEvents` for the new event
- `crates/tetond/src/runtime.rs` — pass `session_id` at the
  `LocalEngineSource::new` call site

## Acceptance Criteria

- [ ] `LocalEngineSource::new` takes the session id; all construction sites
      (runtime, turn_loop, and the in-file tests) are updated
- [ ] `produce_turn` calls `complete_cached` with that key, inside the same
      `spawn_blocking` as today (BR-6)
- [ ] Exactly one `Event::PrefixCache` is emitted per local agent turn,
      carrying `Hit { cached_tokens, new_tokens }` or
      `Miss { reason, processed_tokens }` matching the completion's report
- [ ] A miss reason is never rendered as an error, and an engine error never
      emits a `Miss` event (BR-8)
- [ ] Duty and classify paths still call `complete` and emit no prefix_cache
      event
- [ ] `crates/tetond/tests/nonblocking_inference.rs` passes **unchanged** (AC-7)
- [ ] `cargo test --workspace` passes

## Technical Notes

Do not lock the engine on the async path to read anything — the mutex is held
for the whole of an in-flight completion and a metadata lock there parks a
tokio worker (LESSON-448). The session id is a parameter, exactly as `format`
already is, and for the same documented reason.

The event is emitted **after** the blocking task returns, on the async side,
from the data the completion carried back. Do not emit from inside
`spawn_blocking`.

`SessionEvents` currently exposes only a private `emit` for `SessionUpdate`;
add a narrow accessor or a typed `emit_prefix_cache` rather than making the bus
public.

---
id: TASK-033
title: "Wire ChatFormat through the local source, summarizer, loader report, and smoke"
status: draft
parent: REQ-554
created: 2026-08-03
updated: 2026-08-03
dependencies: ["TASK-030", "TASK-031", "TASK-032"]
---

## Description

Connect the pieces (REQ-554 ADR-2/ADR-3/ADR-4): `LocalEngineSource` caches the
engine's `chat_format()` at construction, renders prompts via
`render_prompt`, and scans replies with `ReplyScanner::for_format`;
`run_session_turn_with_source` builds its `StreamGate` from a new defaulted
`CompletionSource::chat_format()`; `summarize_if_large` wraps its duty prompt
via `render_duty` (BR-7); the engine loader logs the one-line flat-fallback
report for real models (BR-2/AC-3); and a feature-gated `#[ignore]`d smoke
drives one native-template turn on a real GGUF (AC-6).

## Files to Create/Modify

- `crates/tetond/src/harness/completion.rs` — `LocalEngineSource::new` locks
  the engine once to cache `chat_format`; `produce_turn` uses
  `render_prompt(self.format, prompt)` and `ReplyScanner::for_format`; add
  `CompletionSource::chat_format()` (default `Flat`; local override); wiring
  test with `MockEngine::with_chat_format(ChatMl)` + prompt-capturing mock
  asserting the engine receives the ChatML rendering, not `prompt.flat`
  (AC-5's CI pin: the window-checked string IS the rendered one).
- `crates/tetond/src/harness/turn_loop.rs` — `StreamGate::for_format(
  source.chat_format())` in the loop.
- `crates/tetond/src/harness/context.rs` — `summarize_if_large` reads the
  engine's format inside its existing blocking closure and wraps the
  instruction via `render_duty` (BR-7); test with a format-reporting mock.
- `crates/tetond/src/runtime.rs` — at the real-engine install/commit site
  (post-verify loader, ADR-006), when the committed engine reports `Flat`,
  emit exactly one `eprintln!` fallback line naming the model id (BR-2/AC-3).
  Scripted/mock construction does NOT log.
- `crates/tetond/tests/template_smoke.rs` — new, `#[cfg(feature = "llama")]`
  + `#[ignore]`: load `TETON_TEST_GGUF`, assert `chat_format() == ChatMl`,
  drive one rendered turn asking for a `read` tool call, assert `parse_reply`
  finds a single well-formed call (AC-6).

## Acceptance Criteria

- [ ] With `MockEngine::with_chat_format(ChatMl)`, the string handed to
      `Engine::complete` is the ChatML rendering (contains `<|im_start|>`,
      lacks the flat frame); with a default mock it is `prompt.flat`
      byte-identical (AC-1 wiring / AC-5 CI pin).
- [ ] `summarize_if_large` under a ChatMl engine sends a ChatML-wrapped duty
      prompt; under Flat, today's exact prompt (BR-7).
- [ ] The turn loop's gate suppresses `<|im_start|>` fabrication end-to-end
      (AC-4's loop-level half, via the wiring test or reuse of TASK-032's
      gate test at source level).
- [ ] Loader logs the flat-fallback line exactly once for a real engine
      reporting Flat; no log from scripted/mock paths (AC-3) — pinned where
      testable, otherwise asserted by inspection in the PR description.
- [ ] All BUG-147 containment tests and the full existing suite pass
      unchanged (BR-3, AC-7).
- [ ] `cargo test --workspace` and clippy green; the new smoke compiles under
      `--features tetond/llama` (run manually, `#[ignore]`d in CI).

## Technical Notes

- Caching the format at `LocalEngineSource::new` keeps the per-turn path
  lock-free for metadata and makes the mode immutable per source (ADR-2's
  once-per-load resolution; the source is rebuilt per turn-run from the same
  committed engine).
- `CompletionSource::chat_format()` default `Flat` keeps
  `RemoteProviderSource` and test sources unchanged.
- The summarizer's engine lock already happens inside `spawn_blocking` —
  read `chat_format()` there; do not add a second lock on the async path
  (LESSON-448).
- Do NOT touch `last_tool_result_body`, benchmark prompts, or any scripted
  fixture — they are flat-mode by design (architecture Scope notes).

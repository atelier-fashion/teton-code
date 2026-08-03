---
id: TASK-030
title: "ChatFormat enum, pure template detection, and Engine::chat_format() metadata"
status: complete
parent: REQ-554
created: 2026-08-03
updated: 2026-08-03
dependencies: []
---

## Description

Introduce the rendering-mode vocabulary and its detection in `teton-inference`:
a `ChatFormat` enum, a pure `detect_chat_format(template: &str) -> ChatFormat`
matcher (CI-testable — no FFI), a defaulted `Engine::chat_format()` trait
method returning `ChatFormat::Flat`, and `LlamaEngine` detection at load time
via `LlamaModel::chat_template()` (feature-gated). `MockEngine` gains a
`with_chat_format` builder so wiring tests can simulate a template-bearing
engine.

## Files to Create/Modify

- `crates/teton-inference/src/engine.rs` — `ChatFormat` enum (`Flat`,
  `ChatMl`); `detect_chat_format()` pure fn (matches `<|im_start|>` in the
  template string → `ChatMl`, anything else/absent → `Flat`); trait method
  `fn chat_format(&self) -> ChatFormat { ChatFormat::Flat }`; `LlamaEngine`
  reads `self.model.chat_template(None)` in `load()`, stores the detected
  format in a field, overrides `chat_format()`; `MockEngine::with_chat_format`
  builder + stored field + override; unit tests.
- `crates/teton-inference/src/lib.rs` — export `ChatFormat` and
  `detect_chat_format`.

## Acceptance Criteria

- [x] `detect_chat_format` returns `ChatMl` for a ChatML-style template string
      (containing `<|im_start|>`) and `Flat` for an empty/unrecognized one —
      unit-tested without the `llama` feature.
- [x] `Engine::chat_format()` has a `Flat` default: `MockEngine::new`,
      `ScriptedFileEngine`, and every test engine compile unchanged and report
      `Flat`.
- [x] `MockEngine::with_chat_format(ChatFormat::ChatMl)` reports `ChatMl`.
- [x] `LlamaEngine::load` stores the detected format and `chat_format()`
      returns it (compile-verified under `--features llama`; behavior pinned
      by the TASK-033 smoke).
- [x] `cargo test -p teton-inference` green; workspace compiles.

## Technical Notes

- Detection is deliberately a pure function over the template *string* so the
  matcher is testable in CI (REQ-554 ADR-1/AC-8). `LlamaEngine` is only the
  feature-gated caller.
- `chat_template(None)` returning an error (no template metadata) maps to
  `Flat` — never an engine-load failure (BR-6). Keep the reason out of
  `EngineError`; the loader-side fallback log (TASK-033) derives its message
  from the returned format alone.
- Do not touch `Engine::complete` or `GenParams`.

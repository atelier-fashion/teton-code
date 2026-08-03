---
id: TASK-031
title: "First-party ChatML renderer for PreparedPrompt and duty prompts"
status: complete
parent: REQ-554
created: 2026-08-03
updated: 2026-08-03
dependencies: ["TASK-030"]
---

## Description

Create `crates/tetond/src/harness/render.rs`: the pure-Rust prompt renderer
(REQ-554 ADR-1/ADR-3). `render_prompt(format, &PreparedPrompt) -> String`
returns `prompt.flat` for `Flat` and the ChatML rendering for `ChatMl`
(system block, then each role-typed message, ending with the
`<|im_start|>assistant\n` cue). `render_duty(format, instruction) -> String`
wraps a duty instruction (summarizer) as a single user-message conversation
(BR-7). Document the bounded per-message overhead as
`CHATML_PER_MESSAGE_OVERHEAD_BYTES`.

## Files to Create/Modify

- `crates/tetond/src/harness/render.rs` — new module: `render_prompt`,
  `render_duty`, `CHATML_PER_MESSAGE_OVERHEAD_BYTES`, unit tests.
- `crates/tetond/src/harness/mod.rs` — register `pub(crate) mod render;`.

## Acceptance Criteria

- [x] ChatML rendering of a system + user + assistant + tool-result
      conversation contains `<|im_start|>system`, `<|im_start|>user`,
      `<|im_start|>assistant`, `<|im_end|>` delimiters and ends with the
      bare `<|im_start|>assistant\n` cue (AC-1).
- [x] The rendered ChatML string does NOT contain the flat structural frame:
      no `\nUser:\n`, `\nAssistant:\n`, or `\nTool (` block labels (AC-1).
      (Tool-result *content* — including its `<tool-result>` envelope — rides
      inside a user message verbatim, per AC-2.)
- [x] Tool results appear as user-role messages, and consecutive same-role
      messages arrive pre-merged from `prepare()` — the renderer asserts/relies
      on alternation rather than re-merging (AC-2).
- [x] `render_prompt(Flat, p)` returns exactly `p.flat` (fallback is
      byte-identical).
- [x] `render_duty(ChatMl, "Summarize…")` produces a one-user-message ChatML
      conversation ending with the assistant cue; `render_duty(Flat, i)`
      returns `i` unchanged (BR-7).
- [x] Every ChatML message's added delimiter bytes ≤
      `CHATML_PER_MESSAGE_OVERHEAD_BYTES` — pinned by a test that measures
      rendered-minus-content length per message (BR-5 accounting, AC-8).
- [x] All tests run in default CI builds (no `llama` feature) — AC-8.

## Technical Notes

- ChatML shape per message: `<|im_start|>{role}\n{text}<|im_end|>\n`. Roles
  are `system`, `user`, `assistant` (MessageRole::User → `user`,
  Assistant → `assistant`).
- `PreparedPrompt.system` may be empty in tests — render the system block
  only when non-empty (mirror the remote path's `system: Option` handling in
  `completion.rs`).
- Renderer takes `teton_inference::ChatFormat` (TASK-030).
- Do not modify `context.rs` — `assemble()`/`prepare()` and the tests pinning
  `prepared.flat == ctx.assemble()` stay untouched (ADR-3).

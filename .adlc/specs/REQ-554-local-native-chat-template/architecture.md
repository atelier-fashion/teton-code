# REQ-554 — Architecture: native chat template for the local tier

## Approach

The local tier stops hand-feeding llama.cpp a flat `User:/Assistant:` transcript
and instead renders the harness's **already-role-typed** conversation
(`PreparedPrompt.system` + `.messages`, built by `ContextManager::prepare()` for
the remote path since REQ-544 M-8) through the model's native chat template.
For the entire current catalog that template family is **ChatML**
(`<|im_start|>role\n…<|im_end|>\n`).

The pinned `llama-cpp-2` 0.1.151 exposes `LlamaModel::chat_template()` (the raw
GGUF `tokenizer.chat_template` string) and `apply_chat_template()` (FFI over
llama.cpp's fixed-set matcher). We use the **template string for detection
only** and render **first-party in Rust** (ADR-1). Mode is resolved once per
engine load, carried as engine metadata (ADR-2), consumed at the completion
source (ADR-3), and mirrored by the reply scanner's fabrication-marker set
(ADR-4). Flat rendering remains byte-identical as the visible fallback; every
mock/scripted engine stays on it by trait default, so the whole existing e2e
surface is untouched.

## Key decisions

### ADR-1: First-party ChatML renderer; GGUF template used for detection only

**Decision**: prompt rendering is a pure Rust function in the harness
(`harness/render.rs`). The GGUF's embedded template string is read (feature-
gated, via `LlamaModel::chat_template()`) solely to *detect* the family: a
pure `detect_chat_format(template: &str) -> ChatFormat` matches the ChatML
delimiters. `apply_chat_template` (FFI) is deliberately not used.

**Rationale**: AC-8 requires template-mode rendering to be producible and
inspectable in default/CI builds (no `llama` feature). An FFI-only renderer
would satisfy BR-1 but make AC-1/AC-2/AC-5 pinnable only by the `#[ignore]`d
real-model smoke; adding a shadow renderer for CI would mean two renderers
that drift. One Rust renderer serves runtime and tests identically, has no
C-side failure modes on the render path (BR-6 trivially holds — LESSON-444),
and is deterministic. Fidelity to the real model is validated by the AC-6
feature-gated smoke.

**Rejected**: FFI `apply_chat_template` at runtime (+ CI shadow renderer) —
dual-renderer drift; render-path FFI failure modes. Full Jinja execution —
llama.cpp itself doesn't do it; unnecessary for a one-family catalog.

### ADR-2: `ChatFormat` is engine metadata via a defaulted trait method

**Decision**: `teton_inference::ChatFormat { Flat, ChatMl }`. The `Engine`
trait gains `fn chat_format(&self) -> ChatFormat { ChatFormat::Flat }`.
`LlamaEngine::load` detects once (template string → `detect_chat_format`),
stores the result, and overrides the method. `MockEngine` gains a
`with_chat_format` builder for wiring tests. Scripted/gated/test engines
inherit the default and never change.

**Rationale**: the default keeps every existing engine implementor compiling
and on flat rendering — which is what protects `last_tool_result_body`'s flat
parsing (`{{LAST_TOOL_RESULT}}` substitution) and the entire scripted e2e
fixture set without a single edit. Mode is immutable per engine instance
(resolved at load, LESSON-445's stage/commit means the committed engine's
format is the serving format).

**Fallback visibility (BR-2/AC-3)**: the *loader path* in `tetond` (the
post-verify engine install, ADR-006) checks `chat_format()` after a real
engine loads and, on `Flat`, emits exactly one log line naming the model and
reason (`tetond: model <id>: no recognized chat template; using flat
transcript rendering`). Chosen over a new protocol event: the spec allows
either surface; a log line needs no protocol change and satisfies "visible,
once per load". Scripted/mock engines do not log — the report exists for real
models (test doubles are flat by design, not degraded). A future protocol
event can wrap this same site.

### ADR-3: Rendering happens at the completion source; `PreparedPrompt` is unchanged

**Decision**: `harness/render.rs` provides
`render_prompt(format, &PreparedPrompt) -> String` (Flat → `prompt.flat`;
ChatMl → system + each message as `<|im_start|>{role}\n{text}<|im_end|>\n`,
ending with the `<|im_start|>assistant\n` cue) and
`render_duty(format, instruction) -> String` (single user-message wrap for
the summarizer, BR-7). `LocalEngineSource` caches the engine's format at
construction and renders per turn; `summarize_if_large` reads the format
inside its existing blocking closure. `ContextManager::assemble()`/`prepare()`
signatures and outputs are untouched.

**Rationale**: keeps the flat rendering byte-identical (the fallback contract
and the tests that pin it), adds no third shape to `PreparedPrompt`, and puts
the format decision exactly where the engine handle already lives. Budget
note (BR-5): the engine's typed over-window refusal (LESSON-444/446) runs on
the *rendered* string because that is what `complete()` tokenizes — template
overhead is inherently counted. ChatML overhead is bounded and small
(≤ ~40 bytes/message, `CHATML_PER_MESSAGE_OVERHEAD_BYTES` documents it)
against the byte budget's ≥2× headroom (16 KiB budget vs 32 KiB window
currency); the CI pin for AC-5 is a prompt-capturing mock proving the
window-checked string IS the rendered one.

### ADR-4: The fabrication-marker set follows the rendering mode

**Decision**: `ReplyScanner::for_format(ChatFormat)` /
`StreamGate::for_format(ChatFormat)` select the marker set: Flat keeps
`["User:", "Assistant:", "Tool (", "<tool-result"]`; ChatMl uses
`["<|im_start|>", "<|im_end|>", "<tool-result"]`. `<tool-result` stays in
both (the harness's own untrusted envelope is fabricatable in any mode).
Flat markers must NOT fire in ChatML mode (a legit answer may contain
`User:` at a line start). The turn loop learns the mode via a new defaulted
`CompletionSource::chat_format()` (local source returns its cached value;
remote stays Flat). Marker sets are hardcoded per family (OQ-2, resolved).

**Rationale**: BR-4. The JSON tool-call stop is format-agnostic and
unchanged; `<|im_end|>` is normally consumed as an EOG token before any text
is emitted, so its presence in the marker set is defense in depth, not the
primary terminator. BUG-147 containment is preserved verbatim in both modes
(BR-3).

## Scope notes

- **Benchmark prompts** (`teton-inference/src/benchmark.rs::default_prompts`)
  stay flat: the benchmark measures raw completion latency pre-harness, and
  template overhead is immaterial to the BR-8 duty thresholds. Explicitly out
  of scope (the mapper flagged it; declined).
- **Remote path**: untouched (already role-typed).
- **`ContextManager` truncation**: unchanged; headroom analysis above covers
  overhead. If a future template family carries large per-message overhead,
  revisit with a per-format overhead term in `estimated_bytes`.

## Task graph

```
TASK-030 (teton-inference: ChatFormat + detection + trait default)
   ├──> TASK-031 (harness renderer: ChatML render_prompt/render_duty)
   ├──> TASK-032 (mode-aware ReplyScanner/StreamGate markers)
   └──> TASK-033 (wiring: source/summarizer/loader-log/smoke) — depends on 030, 031, 032
```

TASK-031 and TASK-032 are independent of each other (parallel tier).

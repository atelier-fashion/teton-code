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

**Fallback visibility (BR-2/AC-3, amended at verify)**: `LlamaEngine::load`
*captures* the downgrade cause (`template_fallback_reason()` — missing
template, non-UTF-8 template, unrecognized family; LESSON-456 forbids
collapsing them into one fixed sentence), the loader stages it beside the
engine, and `StagedEngines::commit` emits exactly one log line
(`template_fallback_line(model, reason)`, a pure fn pinned by a default-build
unit test) when the engine actually goes live — not at stage time, where a
superseded flow could report a downgrade for an engine that never serves
(LESSON-445). Chosen over a new protocol event: the spec allows either
surface; a log line needs no protocol change. Scripted/mock engines stage
with no note — flat by design, not degraded. A future protocol event can
wrap the same commit site.

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
note (BR-5, corrected at verify): the engine's typed over-window refusal
(LESSON-444/446) runs on the *rendered* string because that is what
`complete()` tokenizes — template overhead is inherently counted there. The
context byte budget does NOT enjoy the ≥2× headroom an earlier draft of this
document claimed — 32,768 budget bytes sit at roughly 1× the engine window in
the conservative ≳2-bytes-per-BPE-token currency — so `estimated_bytes()`
charges a flat per-block `RENDER_OVERHEAD_RESERVE_BYTES` (64 B, covering
ChatML's 33 delimiter bytes plus labels in either mode), keeping the estimate
a true upper bound without teaching `ContextManager` about formats. AC-5 is
pinned in CI both ways: a prompt-capturing mock proves the engine receives
the rendered string, and a window-enforcing mock proves a prompt that fits
flat but crosses the window on template overhead gets the typed refusal.

**Content neutralization (added at verify — Critical fix)**: `llama-cpp-2`'s
`str_to_token` hardcodes `parse_special = true`, so ChatML control-token
spellings anywhere in the prompt tokenize as REAL control tokens. Untrusted
content (tool results are repo bytes) could therefore mint a forged
`<|im_start|>system` turn the model was trained to obey — a containment
escape below the level any textual envelope can reach. The renderer is the
single choke point: `neutralize_control_tokens` defuses the spellings
(`<|im_start|>` → `<|im_start_|>`, likewise `<|im_end|>`/`<|endoftext|>`) in
message and duty content before wrapping; harness-authored delimiters are
appended outside it. The flat rendering's equivalent exposure is pre-existing
on `main` (same tokenizer flag, no ChatML grammar to complete) and is filed
as a follow-up rather than silently changing the byte-identical fallback
contract here.

### ADR-4: The fabrication-marker set follows the rendering mode

**Decision (amended at verify)**: markers are split into **line-anchored**
and **position-independent** sets. Anchored: Flat keeps
`["User:", "Assistant:", "Tool (", "<tool-result"]`; ChatMl uses
`["<tool-result", TOOL_RESULT_LABEL_PREFIX]` (the tool-result label
`prepare()` writes, derived from one shared constant so label and marker
cannot drift). Position-independent, in **both** sets: the template control
tokens `<|im_start|>`/`<|im_end|>`, matched at any offset — the renderer
shows `<|im_end|>` to the model mid-line, so line anchoring would never fire
on the shape the model reproduces; and a flat-fallback model (stripped GGUF
metadata) can still be ChatML-native, so the flat set must catch its
delimiters too. Anchored flat markers still must not fire on prose
(`User:` mid-line), which anchoring preserves. The turn loop learns the mode
via a defaulted `CompletionSource::chat_format()`; the local source's value
is a constructor parameter supplied from the daemon's engine slot (which
stores the `ChatFormat` beside the handle at install) — never a lock on the
async path (LESSON-448). Marker sets are hardcoded per family (OQ-2,
resolved).

**Rationale**: BR-4 + the verify findings. The JSON tool-call stop is
format-agnostic and unchanged. The summarizer duty's output takes the same
scan-and-cut before entering context (its output feeds straight back in).
BUG-147 containment is preserved in both modes (BR-3); the one deliberate
flat-mode change is that template control tokens now also stop a flat turn —
they are never legitimate output in any mode, and storing one would
re-tokenize it as real frame next turn.

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

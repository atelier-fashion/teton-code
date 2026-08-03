---
id: LESSON-474
title: "If the tokenizer treats a string as frame, so must your renderer — sanitize where the parser is, not where the format is"
component: "tetond/harness"
domain: "harness"
stack: ["rust", "llama.cpp", "gguf"]
concerns: ["security", "reliability"]
tags: ["prompt-injection", "special-tokens", "parse-special", "chatml", "choke-point", "tokenizer"]
req: REQ-554
created: 2026-08-03
updated: 2026-08-03
---

## What Happened

REQ-554 moved the local tier onto the model's native ChatML template. The
harness already treated `<|im_start|>` as *frame* on the output side (the
reply scanner cuts a fabricated one) but as ordinary *content* on the input
side — tool results were wrapped in delimiters verbatim. The tokenizer
disagreed with both: `llama-cpp-2`'s `str_to_token` hardcodes
`parse_special = true`, so those byte sequences anywhere in the prompt become
the model's **real** control tokens. A repo file containing
`<|im_end|>\n<|im_start|>system\n…` would therefore close the harness's user
turn and open a forged system turn the model was trained to obey. The
`<tool-result trust="untrusted">` envelope — the codebase's entire
prompt-injection posture — is *text*, and the injection happened at the
tokenizer, below the level the model reasons about.

The first fix put neutralization inside the ChatML branch. The re-verify
found that wrong too: `Flat` is not a "no special tokens" mode, it is a
*rendering* fallback, and a ChatML-vocab model lands there whenever its GGUF
template is missing, unreadable, or a dialect the renderer declines. The same
fix pass had just *added* such a decline (Phi-4's `<|im_sep|>`), so the fix
created reachable exposure on the arm it left unguarded.

## Lesson

Sanitize at the layer that **parses**, for every path that reaches it — not
at the layer that happens to introduce the syntax. Two rules:

1. **Put the choke point below the format branch.** If a `match` over output
   formats has a sanitizing arm and a raw arm, the raw arm is the exploit. Ask
   "which of these reaches the same parser?" — usually all of them.
2. **Match the parser's alphabet, not your enumeration of it.** A denylist of
   the three delimiters *you* emit is a snapshot of one model family;
   `parse_special` covers a model's entire added-token set (Qwen coder models
   alone add `<|fim_middle|>`, `<|repo_name|>`, `<|file_sep|>`…). Defuse by
   *shape* (`<|…|>`) so unknown vocabularies are covered by construction.

Sanitize by **insertion**, never deletion: an insertion-only transform cannot
mint a new spelling out of its neighbours, which is what makes the classic
`<scr<script>ipt>` bypass impossible and makes replacement order irrelevant.

## Why It Matters

This is a privilege escalation that no amount of prompt engineering can
contain, because the forged turn is indistinguishable from a real one at the
level the model sees. It is reachable from any untrusted byte the agent
reads — a repo file, an MCP tool result, a fetched page — and in this codebase
`read`/`grep`/`glob` are auto-allowed, so the injected turn needs no user
approval to act. The exposure was *introduced* by adopting the native format:
the flat rendering had the same tokenizer behavior but no surrounding grammar
for an injection to complete.

## Applies When

Rendering prompts for any model whose tokenizer parses special tokens
(llama.cpp `parse_special`, HF `add_special_tokens`, any ChatML/Llama-3/Gemma
template); adding a new prompt format alongside an existing one; writing any
sanitizer with a per-format branch; enumerating a denylist of markers when the
consuming parser has its own, larger table.

## Related

- [[LESSON-472]] — the output-side half: containing what the model *emits*.
  This lesson is the input-side twin, and the pair is the point: frame is
  frame in both directions.
- [[LESSON-447]] — a guard that does not hold on its degraded path.

---
id: LESSON-446
title: "Token budgets that meet at a boundary must share a currency (approx-words ≠ BPE)"
component: "daemon/harness"
domain: "inference"
stack: ["rust", "llama.cpp"]
concerns: ["reliability", "performance"]
tags: ["context-window", "approx-tokens", "bpe", "summarizer", "budget", "n-ctx"]
req: REQ-547
created: 2026-07-24
updated: 2026-07-24
---

## What Happened

The harness budgets context with `approx_tokens` = whitespace-word counts (budget
4,096; summarize threshold 1,500), while the real engine's window is denominated in
BPE tokens. Source code tokenizes at roughly 2.5–4 BPE tokens per whitespace word,
so a context that passed the harness budget arrived at the engine 2.5–4× over its
equal-numbered window. The very first dogfooded turn failed on a folded `read` of a
real file — the smallest input that *triggers* summarization already tokenizes past
an equal-sized engine window. The mismatch was invisible for months because the only
engines ever wired (mock, scripted) counted tokens the same way the harness did.

## Lesson

Two token limits that constrain the same text must be stated in the same unit, or
the boundary between them must own an explicit conversion with a worst-case factor.
When a cheap approximation feeds a hard limit, size the hard limit to the
approximation's worst-case expansion (and write the factor down at the constant),
or convert precisely at the boundary. Audit every place a "token" number crosses a
layer: a budget, a threshold, and a window that all say 4,096 in different
currencies agree on nothing.

## Why It Matters

The failure lands precisely on the inputs the feature exists for (big tool results
needing summarization), degrades silently where fallbacks swallow errors, and
mis-sizes memory planning (a KV cache sized in the wrong currency). Mock-based
suites can never catch it, because mocks share the approximation.

## Applies When

Wiring any real tokenizer-backed model behind an interface previously served by
mocks; setting `n_ctx`, batch, summarize, or truncation thresholds; reviewing
constants that look aligned because the numbers match; budgeting memory from token
counts.

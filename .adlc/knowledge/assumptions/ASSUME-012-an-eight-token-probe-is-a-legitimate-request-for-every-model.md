---
id: ASSUME-012
title: "An 8-token completion is a legitimate request for every provider kind and model the connection test may be pointed at"
status: unresolved
req: REQ-581
created: 2026-08-17
resolved:
---

## Assumption

`provider/test` sends one fixed request — `PROBE_PROMPT` ("Reply with the
single word OK.") with `max_tokens = PROBE_MAX_TOKENS` (8), no system prompt,
no tools, no reasoning-effort field — and treats the vendor's answer as
evidence about the *connection*: a completion is `reached`, a 4xx is a typed
refusal. REQ-581's spec assumed (Assumptions §1) that every supported provider
kind accepts such a request as legitimate; OpenAI-compatible and Anthropic
both do for ordinary chat models, and the e2e/mock and one live Kimi
(`kimi-k3`) `reached` bear that out.

## Context

The correctness review (REQ-581 verify, Nit 14) named the gap: a
reasoning-first model (the DeepSeek-reasoner-class rows already in the price
table, and any vendor that spends the output budget on hidden reasoning
before emitting a token) may either reject a budget this small (→ a 400 →
`refused { 400 }`, a false negative on a working connection) or spend the
whole 8 tokens on reasoning and return no visible text (→ still `reached`,
since usage is non-zero, but with an empty answer nobody reads). Neither
breaks the *connection* claim — the wire, the credential and the model name
were exercised either way — but the first misreports it, and the fixed budget
was chosen for cost, not for vendor coverage.

Accepted for v1 because every recipe the catalog ships (Anthropic, OpenAI,
Moonshot/Kimi, DeepSeek chat, Grok, Ollama) is a chat model, and because the
typed outcome makes a false negative *legible* (a `refused { 400 }` on a
provider a turn then serves is a visible contradiction), not silent.

## Resolution

(unresolved — revisit when a reasoning-first model is registered as a
provider and tested; the fix, if the 400 shape appears, is either a per-kind
or per-model probe budget, or classifying a 400 whose body names
`max_tokens` the way `EffortRefused` already classifies one that names the
effort field — the adapters already read the error head for that)

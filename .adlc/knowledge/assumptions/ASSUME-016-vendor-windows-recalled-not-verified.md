---
id: ASSUME-016
title: "The context windows named in a spec draft are the windows the vendors actually serve"
status: invalidated
req: REQ-586
created: 2026-08-19
resolved: 2026-08-19
---

## Assumption

REQ-586 was drafted, validated and task-planned against working figures of
128k–200k tokens: the spec's cost reasoning, its worked examples, OQ-6's
resolution ("the window the user declared is the consent" — no default cap),
and several task fixtures all used them. The Anthropic adapter's own
`DEFAULT_MAX_CONTEXT` constant said 200,000, which reinforced it.

## Context

The REQ needed per-vendor windows for the recipe catalog. The numbers went
into the spec from recall — the author's and the model's — because they are
the kind of fact everyone believes they know. REQ-577's rule already says a
fact a catalog ships about an external system is verified against **both**
halves of its contract; that rule was applied to endpoints and example models
and not, initially, to windows.

## Resolution

**Invalidated on every vendor that mattered.** TASK-188 verified each against
the live vendor documentation and cited the URL per entry: Anthropic
claude-opus-5 **1,000,000** (not 200,000); OpenAI gpt-5.6 **1,050,000**;
Moonshot kimi-k3 **1,000,000** (the draft anticipated 128k); DeepSeek
deepseek-v4-pro **1,000,000**; xAI grok-4.6 **500,000**; Ollama llama3.2
**4,096** — the *served* default, not the model card's 128k, and the only one
smaller than Teton's own local pair.

The blast radius was larger than the numbers. The 8× move on the headline
vendors invalidated the premise under which OQ-6 had been resolved: the worst
case per prompt went from ≈3.2M to ≈25M input tokens, which the owner then
amended (TASK-194: a notice when a big window is recorded, still no default
cap). Ollama's 4,096 exposed a fail-open branch in the derivation — a window
below the generation reservation returned the *default* pair — and forced a
floor. And the adapter constant that reinforced the wrong figure turned out to
be dead code: it is overridden at construction by whatever the config record
carries, which defaults to `0`.

**The rule that generalises**: a number a spec reasons *about* is a fact about
a third party, and REQ-577's both-halves verification applies to it exactly as
it applies to an endpoint. Verify before the reasoning is built on it — the
draft's conclusions, not just its tables, depend on it.

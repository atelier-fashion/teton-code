---
id: ASSUME-040
title: "q8_0 KV costs no measurable recall or tool-call accuracy on the shipped 30B"
status: unresolved
req: REQ-616
created: 2026-09-04
resolved:
---

## Assumption

Quantizing the KV cache to `q8_0` has no measurable effect on recall across a
long context, or on tool-call parsing, for `qwen3-coder-30b-a3b`.

## Context

This is the assumption the whole of REQ-616 rests on for the dogfood machine, and
it is load-bearing rather than incidental.

The 48 GiB machine cannot hold the trained 262,144-token window at `f16`: weights
17.3 GiB + KV 24 GiB + compute ≈ 42.3 GiB against a 36 GiB admissible share. At
`q8_0` the same window costs ≈ 30.3 GiB and fits. So on the machine this REQ was
written for, **the full window is only reachable through the quantized cache** —
the probe's `memory_fit` path is not a fallback here, it is the shipped
behaviour.

If the assumption is false, the choice REQ-616 makes is the wrong one: a smaller
`f16` window would be better than a larger `q8_0` one, and `fit_window`'s ladder
(try `f16` at the trained window, then `q8_0`, then step the window down) has its
first two rungs in the wrong order.

## Resolution

**Unresolved. AC-12 was written to test exactly this and was NOT RUN.**

The trial needs the real engine (behind the non-default `llama` feature, which CI
never compiles), 17.3 GiB of weights on disk, and a machine that can hold a
262,144-token context. None of that is reachable from a test run, and the REQ
shipped with the criterion recorded as not run rather than quietly dropped — see
`.adlc/specs/REQ-616-local-engine-full-trained-window/verification-notes.md`,
which carries the procedure.

What *is* verified is adjacent and does not substitute: that the probe picks
`q8_0` at 262,144 on a 48 GiB machine, that the choice is recorded in
`model-selection.toml`, and that both surfaces report it. Those assert the
*decision*, never its effect.

The trial is cheap once the weights are present — plant a fact at the 10 %, 50 %
and 90 % marks of 200,000 tokens of repository context and ask for each, three
runs. Until it is run, the honest statement is that REQ-616's quality claim is
untested and its arithmetic claims are not.

A negative result is a product finding rather than a bug: it would mean
reordering `fit_window`'s ladder to prefer a stepped-down `f16` window over a
full `q8_0` one, which is a change to two rungs and a decision for the product
owner.

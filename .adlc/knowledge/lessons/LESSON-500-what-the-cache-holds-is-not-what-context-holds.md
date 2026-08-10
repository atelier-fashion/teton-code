---
id: LESSON-500
title: "A cache keyed on the conversation must account for what the harness threw away"
component: "inference/local"
domain: "inference"
stack: ["rust", "llama.cpp"]
concerns: ["latency", "correctness"]
tags: ["kv-cache", "prefix-reuse", "truncation", "spec-gap"]
req: REQ-564
created: 2026-08-10
updated: 2026-08-10
---

## What Happened

REQ-564 reuses resident KV when a turn's tokenized prompt extends the resident
prefix. The resident prefix is what the context *decoded*: the prompt, plus
every token generated during the turn. The next turn's prompt is what the
harness *kept*.

Those are not the same string. BUG-147's `ReplyScanner` exists precisely
because weak local models run on past their turn and invent tool results and
future dialogue; `context_cut` drops that continuation before it reaches
context. Every token of it was decoded into the KV first.

So on any turn where the model fabricated, the next turn's prompt diverges from
the resident prefix at the cut point and takes a full cold prefill. The cache
serves well-behaved turns and misses fabricating ones — and the REQ's
motivating measurement (211 context cycles, an 11-generation agent loop on a
weak local model) was *made of* fabricating turns. The optimization is likely
to help least on the workload that justified building it.

Nothing here is a bug. It is BR-2 implemented exactly as written: "Any
divergence … falls back to a full cold prefill from position zero." The rule
was authored before anyone traced where the two token streams part company.

## Lesson

When you cache "the conversation so far", write down **which** conversation.
The model's view (what was decoded) and the harness's view (what was retained)
diverge wherever the harness edits the model's output — truncation, tool-call
extraction, fabrication cuts, compaction, redaction. Any cache keyed on one and
populated from the other silently degrades to a no-op on exactly the turns
where the editing fires, which are rarely the turns you sampled while designing
it.

Trace the two streams to the byte before writing the reuse rule, and prefer a
rule that reuses **up to** a divergence over one that discards everything on
any divergence — the KV truncation primitive supports the former at identical
cost. Where a business rule already mandates the stricter behavior, implement
the rule and surface the finding rather than quietly implementing the better
thing: the spec is where that trade-off belongs, and a silent improvement is
also a silent divergence from the document the next person will read.

Corollary for measurement: settle this before running the benchmark, or the
benchmark will report a disappointing number whose cause is the policy rather
than the mechanism, and the mechanism will get blamed.

---
id: LESSON-532
title: "Presence in context is not instruction-following — a small model transfers data, not directives"
component: "daemon/harness"
domain: "harness"
stack: ["rust", "llama.cpp", "llm-providers"]
concerns: ["developer-experience", "reliability", "latency"]
tags: ["local-tier", "prompt-engineering", "live-ab", "adr-9", "surface-guarantee", "guided-enablement"]
req: REQ-579
created: 2026-08-16
updated: 2026-08-16
---

## What Happened

REQ-579 added `/provider setup` and needed the local model to *say so* when a
user asked to "set up Kimi for deep reasoning" instead of reciting three shell
commands. Three live rounds against qwen3-coder-30b, isolated daemon, fresh
session per prompt, baseline = the shipped binary (verification.md §1–§26):

| Round | Guide change | Hand-off | Side effects |
|---|---|---|---|
| 1 | hand-off in the preamble; step 1 leads with the CLI | 0/3 | model tries to `shell` the CLI 3/3 |
| 2 | hand-off **inside** the numbered step, `Shell only:` marker | 0/3 | no shell probes; knows the command when asked directly |
| 3 | vendor recipes removed from the guide entirely | 0/3 | shell probes back 3/3, model calls doubled, fetched topic ignored, hallucinated `/provider setup kimai build` |

Every round the **data** crossed perfectly — the exact endpoint, the exact
example model, no fabrication. The **instruction** never did, even when the
model itself fetched the docs topic that opens with it. What shipped is round
2's guide plus a deterministic surface guarantee (ADR-9): when a TTY reply
recites the CLI, the harness appends one `>>` line naming the command.

## Lesson

For a small local model, presence in context buys retrieval, not compliance.
It reliably extracts facts it can see; it follows the most concrete, familiar
recipe it can see. Moving the sentence, dictating it, putting it inside the
governing step, or removing the competitor are all the same lever, and the
lever does not move this. When a UX transition *must* happen, put the
guarantee where a test can pin it — the surface, keyed on the model's own
output — and let the prompt be the nice-to-have. Stop after two prompt rounds
if the second is not measurably closer; the third here made five things worse.

## Why It Matters

Each prompt revision is a behaviour change with its own regressions (tool
probes, doubled calls, hallucinated forms), and each live round costs a
full llama rebuild plus a 30B load. A deterministic affordance costs one
string match, is mutation-testable, and goes dormant on its own the day a
model does the right thing. ASSUME-008 is now resolved-with-a-split on this
evidence: reference data in a topic is supported; instruction-shaped content
in a topic is not.

## Applies When

- Designing a hand-off from chat to a structured command on the local tier.
- Reaching for "the model will say X if we tell it to" as an acceptance
  criterion — make the surface half the pass condition and record the model
  half honestly.
- Deciding whether to spend a third live A/B round on wording.

---
id: ASSUME-019
title: "MCP tool calls and the titling duty are outside the per-prompt spend ceiling"
status: open
component: "egress"
domain: "cost"
stack: ["rust"]
concerns: ["budget-accuracy"]
tags: ["mcp", "titling", "spend-ceiling", "req-588"]
req: REQ-588
created: 2026-08-23
updated: 2026-08-23
---

## The assumption

Two remote paths are deliberately not measured against the ceiling, each for its
own reason:

- **MCP tool calls** carry no `CostAttribution` — no model, no priced token — so
  they can neither add to a prompt's spend nor be measured against it. A ceiling
  there would advertise a check that could never fire.
- **The titling duty** is spawned detached and can outlive the prompt that
  triggered it. Binding it to that prompt's accumulator would let a background
  job spend against a total nobody is watching, and would race the next prompt's.

## Why it is an assumption and not a fact

Both rest on today's shape, and both could stop holding without anyone noticing:

- If MCP calls ever gain cost attribution — a priced remote MCP service, say —
  they become real spend that the ceiling silently does not see. The ceiling
  would then under-count, which is the failure mode a budget feature can least
  afford.
- Titling is assumed **cheap**. Nothing enforces that. A titling duty routed to
  an expensive tier would spend outside any ceiling, indefinitely.

## What would falsify it

- Any `CostRecord` appearing in the ledger whose call did not pass a ceiling
  check while a ceiling was configured.
- A `/cost` report where the per-prompt total plus the ceiling refusals do not
  account for the session's spend.

## If it turns out wrong

Both are fixable in the same shape the main path already uses: give the call an
attribution and hand it the prompt's accumulator. Titling additionally needs a
decision about *which* prompt a detached job belongs to — the reason it was left
out rather than a reason it must stay out.

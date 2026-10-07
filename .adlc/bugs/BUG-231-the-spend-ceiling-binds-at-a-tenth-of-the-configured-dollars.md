---
id: BUG-231
title: "The per-prompt spend ceiling binds at a tenth of the configured dollars — config micro-cents vs recorded usd_micros"
status: open
severity: high
created: 2026-10-07
updated: 2026-10-07
component: "daemon/cost-ledger"
domain: "cost"
stack: ["rust", "daemon"]
concerns: ["cost", "reliability"]
tags: ["spend-ceiling", "units", "micro-cents", "usd-micros", "prompt-ceiling", "req-588", "req-623"]
introduced_by: ["REQ-588"]
attribution: manual
---

## Description

`CostConfig::ceiling_micro_cents()` converts `[cost] prompt_ceiling_usd` at
100,000 units per USD, but every recorded call cost is `usd_micros`
(1,000,000 per USD) and is added to `PromptSpend` unconverted. The ceiling
therefore trips at roughly a tenth of the figure the user wrote. Found by
TASK-424 while wiring `spent_by_child`, and confirmed in a probe during
TASK-430: a $0.44 call was reported as "spent $4.40" against a $0.05 ceiling.

REQ-623's `SharePool` deliberately reuses the same `spend_units` helper so
child shares and the ceiling check agree with each other; fixing the unit at
that one seam fixes both.

## Reproduction Steps

1. Set `[cost] prompt_ceiling_usd = 1.0` with a priced remote provider.
2. Make calls totalling about $0.11.
3. The next call is refused `SpendCeilingReached`, and the refusal sentence
   reports the spend as ~$1.10.

## Expected Behavior

The ceiling binds at $1.00 of recorded spend, and the refusal reports the real
figure.

## Actual Behavior

It binds at ~$0.10, and the reported spend is 10× the real one.

## Environment

- Platform: all
- Version: since REQ-588 (v0.1.3x); still present at REQ-623's merge (919bccc)

## Root Cause

(filled during investigation — the candidate is the 1e5 vs 1e6 constant at the
`ceiling_micro_cents()` / `spend_units` seam; `teton_core::cost_ceiling::usd`
renders with the same assumption)

## Resolution

(filled after fix)

## Files Changed

- (none yet)

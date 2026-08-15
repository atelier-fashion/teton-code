---
id: ASSUME-009
title: "A flat per-model rate can represent vendor billing"
status: invalidated
req: REQ-544
created: 2026-08-14
resolved: 2026-08-14
---

## Assumption

Every vendor's API billing can be expressed as one pair of integers per model:
micro-USD per Mtok input and per Mtok output. The `prices.toml` schema, the
`ModelPrice` struct, and the cost formula all rest on this.

## Context

Made implicitly when REQ-544 built the cost meter, and it held for every
vendor at the time. The whole ledger pipeline — pricing at egress, the savings
estimate, the report arithmetic — consumes exactly two rates per model.

## Resolution

Invalidated during the 2026-08-14 price-table sweep (REQ-577 follow-up,
PR #148). Two current vendors bill in ways the schema cannot express:

- **DeepSeek** switches to peak/off-peak time-of-day billing on 2026-08-16
  16:00 UTC (peak = 2x off-peak). A flat row is thereafter an approximation
  whichever figure it carries.
- **xAI** bills `grok-4.6` requests with prompts ≥200k tokens at 2x the
  standard rate for *all* tokens in the request — a per-request tier, not a
  per-model rate.

Current handling: rows carry the standard/flat rate with the caveat documented
in `prices.toml` comments, and a follow-up exists to pick a documented
convention for DeepSeek after the switch. If time-of-day or tiered pricing
spreads to more vendors, first-class schema support becomes a REQ-sized
decision rather than a data edit.

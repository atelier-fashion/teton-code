---
id: BUG-193
title: "The prompt-margin ledger drifts silently while its test stays green"
status: open
severity: medium
created: 2026-08-26
updated: 2026-08-26
component: "daemon/egress"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability"]
tags: ["system-prompt", "budget", "redact", "doc-comment-drift", "stale-ledger", "margin", "req-592", "test-cannot-fail"]
---

## Description

`crates/tetond/src/egress/redact.rs`'s `REDACT_BODY_OVERHEAD_BYTES` carries a doc-comment ledger
recording how much room the resident system prompt has left. It says:

> 710 bytes of filler leaves this shape at exactly the 48-byte floor and passes, 711 fails.

That figure dates from REQ-587. **The real margin before REQ-592 was 476 bytes** — the worst-case
prompt has grown roughly 234 bytes since, across REQ-583, REQ-585, REQ-587, REQ-589, REQ-590 and
REQ-591, without the ledger being restated once.

REQ-592 then spent 347 of those on `OUTPUT_FORMAT_CLAUSE`, leaving **129 bytes against the
48-byte floor — 81 bytes of usable room.**

## Why it drifts silently

This is the interesting half, and it is why the fix is not just "edit the number".

`the_total_cap_clears_the_harness_context_budget_with_margin` asserts:

```rust
assert!(margin >= MIN_PROMPT_HEADROOM_BYTES, …)
```

An inequality. It holds identically at a margin of 710, 476, or 49 — so the test stayed green
through every one of those six REQs while the documented figure grew more wrong. The number that
tells a human how much room is left lives in prose, and nothing compares the prose to reality.

The ledger's own doc comment is meticulous about *recording* each raise (REQ-577, BUG-181,
REQ-587 all have entries). What no one did was re-measure the margin when the prompt grew without
the constant moving — which is the common case, and the one the ledger is least equipped to catch.

## Impact

Anyone sizing a new prompt clause reads 710 and believes they have ~660 bytes of headroom. They
have 81. The failure is not silent when it lands — the margin test fails the build — but it is
discovered at the end of an implementation task rather than at the start of a design decision,
which is exactly backwards for a constant whose raise has a documented second-order cost
(REQ-586 gave it a production reader, so raising it narrows every `[privacy] redact = true`
route's scannable budget).

REQ-592 hit this: the task brief was sized against 710, and the implementer had to correct it
mid-task and report the discrepancy.

## Reproduction

1. Read the doc comment on `REDACT_BODY_OVERHEAD_BYTES` (`crates/tetond/src/egress/redact.rs`,
   around line 2273).
2. Apply its pad method — the one `docs/manual-verification.md` records — to
   `the_total_cap_clears_the_harness_context_budget_with_margin` on current `main`.
3. Observe the boundary is not 710.

## Expected

The recorded figure matches the measured one, and a drift of ~234 bytes is visible to someone who
did not go looking for it.

## Suggested fix

Two parts, and the second is the one that matters.

1. **Restate the ledger** with the current measured figure, adding REQ-592's clause to the entry
   list. Re-measure with the pad method rather than trusting any number in this bug report.

2. **Consider pinning the margin** so it cannot drift again — assert it against an expected value
   updated deliberately (the way the ledger entries themselves are), rather than only against the
   floor. Then a prompt that grows 234 bytes turns a test red instead of leaving a comment wrong.

   This is a genuine trade, not an obvious win: a pinned margin means churn on every prompt edit,
   including edits that have nothing to do with the budget. Weigh it. A middle option is to assert
   the margin falls in a band, wide enough to absorb ordinary edits and narrow enough that six
   REQs of drift cannot pass through it.

## Notes

Found during REQ-592 implementation (2026-08-26). REQ-592 corrected its own spec and left
`redact.rs` untouched as out of scope; this is that deferral, filed.

Related: [[LESSON-481]] (a gate that hides a feature also hides its tests) is the nearest sibling —
here it is an *inequality* rather than a gate that hides the regression, but the shape is the
same: the assertion that passes is not the assertion anyone believes is being made.

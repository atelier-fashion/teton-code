---
id: BUG-193
title: "The prompt-margin ledger drifts silently while its test stays green"
status: resolved
severity: medium
created: 2026-08-26
updated: 2026-08-27
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


## Resolution (2026-08-27)

**Both halves fixed, and the second one is the fix that matters.**

**Restated.** The measured figures, taken with a temporary probe and reverted:
`worst` 7,859 + `escaping` 3,276 = `spent` 11,135 against 11,264 → margin **129**. The twin in
`harness/tools/web.rs` measures the web-enabled shape: `spent` 11,088 → margin **176**. The stale
"710 bytes of filler" sentence is replaced with the measurement plus the drift history; the twin's
own stale "757 bytes of filler" sentence got the same treatment, and it gained the
`Recorded headroom at REQ-592` ledger line that REQ-592 never added.

**Pinned.** `RECORDED_PROMPT_MARGIN_BYTES = 129` and `RECORDED_WEB_PROMPT_MARGIN_BYTES = 176`, both
asserted with `assert_eq!` beside the existing floor. The floor answers "is there room at all";
the pin answers "did the resident prompt move without anyone noticing", which is the question that
actually went unasked for six REQs.

**The churn trade, decided rather than dodged.** Every intentional prompt edit now fails a test
until the number is updated. That is the point: 129 against a 48-byte floor is **81 bytes of usable
room**, so an edit costing 20 bytes is a fifth of what remains and should announce itself. At 710
this pin would have been noise; at 81 it is the cheapest possible alarm. The failure message says
what to do — re-measure, add a ledger line naming the REQ, move the constant in the same diff — and
tells the reader explicitly **not** to widen it back into an inequality, since that is the shape
that allowed the drift.

**Mutation-checked, both directions.** Bumping the pin 129 → 130 fails with the full remediation
message. More importantly, adding a **single byte** to the real `OUTPUT_FORMAT_CLAUSE` fires *both*
pins, each reporting `a change of -1`. A one-byte drift is now caught; the 234-byte drift this bug
is about would have been unmissable.

**Scope note.** The bug named `redact.rs`. The twin in `web.rs` carried the same defect — a live
figure in prose that nothing compared to reality — so both were fixed. Fixing one would have left
the identical bug in the sibling surface (LESSON-525's cross product).

1,910 `tetond` tests pass, clippy `--all-targets` and fmt clean.

---
id: LESSON-488
title: "A timeout is a drop, and a dropped stream never bills"
component: "daemon/cost"
domain: "cost"
stack: ["rust", "tokio", "daemon"]
concerns: ["cost", "async", "verification"]
tags: ["billing", "tokio-timeout", "drop-semantics", "metered-body"]
req: REQ-561
created: 2026-08-07
updated: 2026-08-07
---

## What happened

An implementer was asked to bound an unbounded duty await two ways: add a
deadline, and break out of stream accumulation once the ceiling is reached. It
**declined the second half** with a specific argument — `MeteredBody::finalize()`
runs only on `Poll::Ready(None)` and there is no `Drop` impl, so breaking the
stream early would drop it before the call was ever billed. With `title`'s
128-byte ceiling, "overran its ceiling" and "was never billed" would have been
nearly the same set. Verified: the claim held.

Then the accepted half did the same thing on a different trigger, and it took the
Phase-5 **confirmation loop** to catch it. `tokio::time::timeout` *drops* the
inner future on expiry. That future owns the `TurnStream`, hence the
`MeteredBody`. So a remote duty exceeding the deadline sent its request, spent
the tokens, and wrote no ledger row — the exact hole whose avoidance had been
praised one commit earlier.

The remedy was `impl Drop for MeteredBody`. But the literal version was also
wrong: provider adapters reject on status *before* reading a byte, then drop the
body unpolled, so an unconditional `Drop` bills every 4xx/5xx as a 0-token row
and inflates `CostReport::calls` — changing what a ledger row *means* as a side
effect. Gated on a `polled` flag set on `poll_next` entry.

## Why it matters

In async Rust, **every timeout is a cancellation, and every cancellation is a
drop**. Any bookkeeping that lives in a stream's terminal branch is skipped. The
failure is silent and points the wrong way: the call looks like it never
happened.

The second-order point is sharper. Recognising a hazard once does not inoculate
you against reintroducing it through a different mechanism a commit later. Only
the confirmation loop — re-reviewing the *fixes*, not the original diff — caught
it.

## How to apply

When adding a timeout, cancellation, or early `break` around a stream, ask what
runs only at `Poll::Ready(None)` and confirm a `Drop` path covers it.

Do not skip Phase-5's Step-D confirmation pass on the grounds that the fixes were
small. Fixes are written under time pressure, against a narrower reading than the
original code got.

And know the limit of the remedy: `Drop` makes a cut-off call **visible**, not
**fully priced** — both provider families report output tokens only in the final
chunk. Say which one you mean.

Related: [[LESSON-441]], [[LESSON-483]].

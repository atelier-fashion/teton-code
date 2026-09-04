---
id: ASSUME-041
title: "75 % of physical RAM is the right admissible share for the local engine"
status: unresolved
req: REQ-616
created: 2026-09-04
resolved:
---

## Assumption

The daemon may plan to occupy 75 % of physical RAM, and the remaining quarter is
enough for the user's own work.

## Context

REQ-616's OQ-1 asked whether the existing model-selection rule already carried a
headroom figure to reuse. **It does not**, and finding that out is half of why
this assumption exists. `ModelEntry::ram_floor_bytes` is a per-model *minimum-RAM
gate* — "is this machine big enough for this model at all" — not a budget of
bytes the process may spend. The two answer different questions and only one of
them was ever asked.

So the fraction is new, and it is a judgement. What constrains it is arithmetic
rather than taste: AC-5 requires a 48 GiB machine to **admit** `q8_0` at the
trained window (30.3 GiB resident) and **refuse** `f16` (42.3 GiB), which puts
the fraction in `[62.5 %, 87.5 %)`. 75 % is the midpoint of that band and leaves
12 GiB on the dogfood machine.

Two things make it more than an arbitrary pick inside a range, and both are worth
recording because they are the first things to re-examine if it turns out wrong:

- The band's *width* is an accident of one machine and one model. A different
  RAM figure or a different weight size moves both endpoints, and nothing
  guarantees a midpoint stays sensible.
- `ram_floor_bytes` is **already** mildly inconsistent with the KV measurement:
  the 30B's 20 GiB floor less its 17.3 GiB of weights leaves 2.7 GiB, against the
  3.0 GiB the cache measures at the *current* 32,768 window. REQ-616 deliberately
  did not touch it, because changing it changes model *selection*, which the REQ
  puts out of scope. That leaves two numbers describing overlapping facts and
  disagreeing slightly — a seam worth closing in whichever REQ next touches
  selection.

## Resolution

**Unresolved.** Nothing in this REQ measures whether a machine at 75 % occupancy
is still pleasant to use, and the figure has not been exercised outside the four
synthetic cases in `fit_window`'s table test.

The observable that would settle it is memory pressure on the dogfood machine
during a long session with a 262,144-token context resident — 30.3 GiB of 48 —
alongside an editor and a browser. `pressure.rs`'s runtime watcher already exists
and would be the place to read it from rather than a manual observation.

If it is too generous the symptom is swapping under ordinary use, which the
watcher should catch and downgrade on; if it is too conservative the symptom is
`local_window_refused` on machines that could in fact have served the window, and
the event carries the arithmetic to prove it.

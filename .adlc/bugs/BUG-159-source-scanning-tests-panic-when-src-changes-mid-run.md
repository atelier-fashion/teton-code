---
id: BUG-159
title: "Source-scanning tests panic when src/ changes mid-run — which is exactly what a mutation pass does"
status: open
severity: medium
created: 2026-08-07
component: "daemon/tests"
domain: "verification"
found_by: REQ-561 Phase-5 confirmation pass
---

## What happens

`call_sites.rs`'s `production_source` reads each file it walks:

```rust
let text = std::fs::read_to_string(path).expect("readable source file");
```

The walk and the read are separate steps, so any writer touching `src/` in
between panics the test. Two modules scan production source this way —
`crates/tetond/src/call_sites.rs` and `crates/tetond/src/harness/duty.rs` — and
between them they own five tests:

- `call_sites::tests::the_unreached_marker_matches_the_daemons_actual_call_sites`
- `harness::duty::tests::no_duty_module_carries_any_of_the_seams_concerns`
- `harness::duty::tests::one_route_type_one_trait_and_two_implementations_serve_every_duty`
- `harness::duty::tests::the_duty_path_has_one_egress_scoping_call_and_one_ceiling_site`
- `harness::duty::tests::no_duty_category_is_ever_produced_from_text`

Reproduced deliberately with a loop creating and removing one `.rs` file under
`src/`: **11 failures in 24 runs**, every one panicking at
`call_sites.rs:119:50`.

## Why this matters more than a flaky test usually would

**It fires precisely during a mutation pass.** This repo verifies changes by
applying a mutation, running the suite, observing red, and reverting — the
convention LESSON-441 exists to enforce. That workflow rewrites a source file
between `cargo test` invocations, which is exactly the race above.

So the failure mode is: a mutation pass produces a cluster of red tests that have
nothing to do with the mutation, in the two modules most likely to be involved in
a seam change. It looks like a real finding and is not.

This is not hypothetical. During REQ-561's Phase-5 confirmation pass, two
reviewers independently reported unreproducible multi-test failure clusters under
load, one specifically naming `call_sites` and the duty-seam tests. That shape is
explained by this bug. (One reported symptom — a `RouteDecided` value mismatch of
`Compact` vs `Title` — is **not** explained by it and remains open; every
dispatch test builds its own `EventBus` and drains it synchronously, and no
mechanism was found.)

The cost is compounding: it makes the repo's primary verification technique
occasionally lie, in the direction of a false positive, which is the direction
that wastes the most time.

## Suggested fix

Two lines: have `production_source` skip a file that vanished or became
unreadable mid-scan rather than `expect`-ing it, and re-walk or fail with a
message naming the race instead of a bare "readable source file".

Do **not** weaken the scan's deliberate loud-failure posture for anything else —
the `expect` on a file that genuinely should exist is correct, and the doc
comment above it ("rather than pass wrongly") is the right instinct. Only the
concurrent-modification case should be tolerated, and it should say so.

Worth adding: a test that removes a file mid-walk and asserts the scan reports
the race rather than panicking on an unrelated line.

## Related

- LESSON-441 (a deletion is verified only by proving restoration breaks
  something — the workflow this bug interferes with)
- REQ-561 Phase-5 confirmation pass, where it was found and reproduced

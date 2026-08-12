---
id: BUG-159
title: "Source-scanning tests panic when src/ changes mid-run — which is exactly what a mutation pass does"
status: resolved
severity: medium
created: 2026-08-07
updated: 2026-08-12
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

## Root Cause

A **listing and the reads that follow it are separate syscalls**, and the scan
treated the listing as still-true. `rust_files` walks `src/`, then each path is
opened a moment later; anything that rewrites `src/` in between — an editor
saving by atomic rename, a `git checkout`, or a mutation pass — makes an opened
path `NotFound` and the `expect` turns that into a panic attributed to whatever
test happened to be sweeping.

The race has **three** windows, not the one the report names:

1. `production_source`'s read (`call_sites.rs`, the reported line).
2. `rust_files`' own recursion — `path.is_dir()` is a separate syscall from the
   `read_dir` that follows it, so a *directory* removed mid-walk panics too.
3. Both again in the client twin, `crates/teton/src/status.rs`.

**Partly fixed already, which the report predates.** `production_sources`
(plural) was hardened during REQ-562/REQ-570 with a re-list-and-retry — so the
four `harness::duty` tests listed above were no longer affected by the time this
was picked up. Only `call_sites`' own sweep still used the racy singular read.

## Resolution

The fix follows the report's instruction to keep the loud posture everywhere it
means something, and splits on **who asked for the file**:

- **A sweep** (`production_sources`) skips a file that is genuinely gone — nobody
  asked for it by name, and its absence is a fact about timing, not about the
  source tree.
- **A by-name read** (`production_source`, e.g. `router.rs`) retries once to
  close the atomic-rename window, then **still panics**. A renamed or deleted
  module must fail naming itself, never pass against an empty string.
- **The walk** (`rust_files`, both crates) skips a directory that vanishes before
  it can be descended into. Every other error stays fatal.

`the_unreached_marker_matches_the_daemons_actual_call_sites` now calls
`production_sources()` instead of re-walking with `rust_files` + per-file
`production_source` — a sweep should use the sweep API, which also drops a
duplicated walk.

**The tolerance is floored.** Every skip above can only shrink what a sweep sees,
and the callers are set-based: each missed file makes an "every source has
property P" assertion pass a little more easily, and a scan seeing *nothing*
would pass all of them. Both `production_sources` implementations now assert a
minimum file count, so the race tolerance cannot quietly become a vacuous green.

### Verification

- **Reproduced first**: 4 failures in 24 runs against a concurrent
  create/remove loop on a `.rs` file under `src/`, panicking at
  `call_sites.rs:129` with `readable source file: NotFound` — the report's
  signature at its current line.
- **After the fix**: 0 failures in 40 runs of the same harness; 0 in 12 runs each
  for all five tests the report names.
- **Mutation check** (LESSON-441): making `rust_files` return nothing turns both
  the `call_sites` sweep **and** a `harness::duty` test red on the new floor
  message, rather than passing vacuously — so the floor is load-bearing.
- Full workspace: 2215 passing across 45 targets, `fmt --check` and
  `clippy -- -D warnings` clean.

### Deployment

n/a — this repo is a plain PR-gated OSS flow with no Cloud Run services and no
`gcp:` config (conventions.md, Git Conventions). The fix rides `main` and reaches
users at the next tagged Homebrew release; it is test-harness code, so it changes
nothing in the shipped daemon's runtime behaviour.

Merged in PR #104 (`37bf7c8`), 2026-08-12.

## Files Changed

- `crates/tetond/src/call_sites.rs` — `rust_files` tolerates a vanished
  directory; `production_source` retries the rename window but stays fatal on a
  genuinely missing named file; `production_sources` gains the floor assertion;
  the marker sweep uses `production_sources()`
- `crates/teton/src/status.rs` — the same walk tolerance and floor in the client
  twin

## Related

- LESSON-441 (a deletion is verified only by proving restoration breaks
  something — the workflow this bug interferes with)
- LESSON-489 (a test that reads `src/` races the mutation that tests it — the
  lesson this bug produced, and the basis of the earlier partial fix)
- REQ-561 Phase-5 confirmation pass, where it was found and reproduced

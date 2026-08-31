---
id: LESSON-596
title: "A ratchet guards a spelling, not a property — and the spelling it misses is the wider one"
component: "daemon/runtime"
domain: "testing"
stack: ["rust"]
concerns: ["security", "maintainability"]
tags: ["ratchet", "visibility", "derived-checks", "false-green", "adversarial-review"]
req: REQ-602
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

REQ-602 narrowed `crates/tetond/src/runtime/`'s submodule visibility from 88
`pub(crate)` declarations to 4, and shipped a ratchet to hold the line — bounded
on both sides, with a vacuity floor, four mutations recorded, corpus enumerated
from disk. It was, by the standards of this codebase, a careful guard.

It matched one string: `pub(crate) `.

Review demonstrated the consequence rather than asserting it. Changing
`runtime/taint.rs`'s `pub(super) fn lift` to `pub fn lift`:

- compiles;
- makes the lift reachable from `crate::harness::tools` — which `taint.rs`'s own
  header, twelve lines above, says must be impossible, that being where a
  model's tool call lands;
- leaves the entire suite green, the ratchet included.

The guard written to stop the visibility surface from widening did not notice
the surface widening, because it widened in a spelling the guard was not
watching. `pub` is *wider* than `pub(crate)`, and the ratchet's whole subject was
"nothing gets wider".

Three further instances of the same shape turned up in the same review:

- **Fields.** The parser matched `pub(crate) fn`, not `pub(crate) name:`. The
  diff had narrowed roughly twenty-five struct fields, so the thing the REQ
  actually changed was the thing the ratchet could not re-check. Re-promoting
  `SessionTaintView`'s two fields left every test green — and that type is
  `pub use`-exported, so the fields become readable from the same module
  `taint.rs` says must not reach them.
- **The corpus was a hardcoded file list.** A module added by the next REQ would
  simply not be scanned — and the stated reason for landing this REQ *before*
  that one was that its new modules would inherit these defaults.
- **A sibling parser, written the same day, handled `unsafe`, `union` and
  `extern`, and this one did not.** Two parsers over the same tree, disagreeing
  about what a declaration looks like.

## Lesson

**A ratchet does not guard a property. It guards a spelling of that property,
and everything it fails to spell is a hole with a green test over it.**

The failure is asymmetric in the worst direction. A guard that misses a
*narrower* spelling produces a false positive: someone investigates and fixes
the guard. A guard that misses a *wider* one produces a false negative that
looks exactly like success — and the wider spellings are the ones a regression
actually reaches for, because widening is what regressions do.

So when writing a guard on "nothing gets wider than X":

- **Enumerate the whole lattice above X, not X.** For Rust visibility that is
  `pub(crate)`, `pub(in path)`, and `pub`. Guarding the middle rung only is
  guarding the rung you happened to be editing.
- **Ask what the diff actually touched.** This REQ narrowed ~25 fields and the
  ratchet parsed only functions. The strongest signal for what a guard must
  parse is what the change it accompanies actually changed.
- **Enumerate the corpus from disk.** A hardcoded list is a corpus that stops
  growing while the tree does not — and it goes blind precisely when new code
  arrives, which is when a ratchet is for.
- **Prefer an empty-set assertion where one is available.** "No submodule field
  is `pub(crate)`" is stronger than any allowlist, because there is nothing to
  keep in step.

And the meta-point, which is why this is a lesson and not a bug report: **the
guard was reviewed, mutated, floored, and documented, and it was still watching
the wrong door.** Mutation testing only proves the mutations you thought of.
Every mutation run against this ratchet mutated a `pub(crate)` item, because
`pub(crate)` was the word in everyone's head — the author's included.

## Why It Matters

The invariant at stake is a security one. `taint.rs` exists so a session that
has read boundary content cannot have that taint lifted by the model's own tool
calls. That is stated in prose in the module header and enforced, supposedly, by
visibility. A one-word edit defeats it and nothing goes red.

It also compounds. REQ-600 and REQ-603 both extract new modules into this tree,
and the argument for landing REQ-602 first was that they would inherit its
defaults. A ratchet with a frozen corpus and a single-spelling parser would have
inherited them nothing.

## Applies When

Writing any ratchet, allowlist, or "this set is exactly N" guard — especially
over a *lattice* (visibility, permissions, log levels, trust tiers, HTTP
methods, feature flags), where "no wider than X" has more spellings above X than
the one in front of you. Also when a guard's corpus is a literal list rather
than an enumeration, and when two parsers in the same change set disagree about
what a declaration looks like: see [[lesson-585]] for the vacuity half of this,
[[lesson-594]] for the corpus half, and [[lesson-595]] for the widening it was
built to prevent.

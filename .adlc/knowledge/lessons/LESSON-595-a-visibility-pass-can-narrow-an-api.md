---
id: LESSON-595
title: "A bulk visibility pass can narrow a public API while appearing only to move code"
component: "adlc/verification"
domain: "refactoring"
stack: ["rust"]
concerns: ["maintainability", "correctness"]
tags: ["visibility", "public-api", "re-export", "audit", "vacuous-check", "refactor"]
req: REQ-599
created: 2026-08-31
updated: 2026-08-31
---

## What Happened

Extracting code into sibling modules requires widening visibility — private
items must become `pub(super)` or `pub(crate)` to stay reachable. REQ-599 did
this with a scripted pass per step.

It narrowed the API twice, in two different ways:

1. The pass rewrote `pub struct BoundaryPosture` to `pub(crate)`. Clippy caught
   it: the type is returned by a `pub fn`, so the signature was now more public
   than the type in it.
2. Items that stayed `pub` were re-exported from the parent with
   `pub(crate) use taint::*;`. Since `mod taint;` is **private**, the glob was
   the only path to them, and `pub` on the declaration bought nothing —
   `tetond::runtime::SessionTaint` stopped existing. Three integration tests
   caught this; clippy did not.

Worse, the audit written after the first incident to catch exactly this class
was **vacuous**. It ran `git show <sha>:crates/tetond/src/runtime.rs` against a
commit where the file had already been renamed. `git show` returned nothing, the
comparison ran against an empty list, and it reported "no demotions" — passing
by seeing nothing, one step after a vacuity floor had been added elsewhere to
prevent precisely that.

## Lesson

**Visibility is API surface, and a pass that widens can also narrow.** Two
distinct things must both hold, and neither implies the other:

- the item's own declared visibility, and
- **reachability** — an item in a private module is only as public as the
  re-export that carries it. A `pub(crate) use` glob silently caps everything it
  carries.

Prefer re-exporting the public surface **by name**, so it is a list someone can
read rather than a consequence of where an item happens to live.

And audit against a ref that actually contains the file. A comparison whose
baseline came back empty should fail loudly, not report agreement.

## Why It Matters

This is a change to the crate's contract disguised as a file move. The commit
message says "extract module"; the diff is a relocation; the effect is a
narrowed API. Reviewers read the first two.

No single instrument caught all of it: clippy found the return-type case and
missed the re-export case; the integration tests found the re-export case and
would have missed the other. The bespoke audit written to cover both found
nothing because it was broken.

## Applies When

Extracting code into new modules; writing any bulk `sed`/script pass over
visibility qualifiers; adding a glob re-export from a private module; or writing
a check that compares against a historical revision — assert the baseline is
non-empty before trusting agreement.

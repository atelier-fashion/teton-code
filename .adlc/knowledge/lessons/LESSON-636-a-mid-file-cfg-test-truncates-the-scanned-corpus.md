---
id: LESSON-636
title: "A mid-file `#[cfg(test)]` truncates the corpus every source-scanning check reads"
component: "daemon/testing"
domain: "testing"
stack: ["rust"]
concerns: ["reliability", "maintainability"]
tags: ["source-scanning", "production-source", "cfg-test", "test-placement", "vacuity"]
req: REQ-616
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-616 added a `#[cfg(test)] mod inference_config_tests` to `runtime/mod.rs`,
placed next to the function it tested — about 100 lines above `apply_update`.

`child_env`'s security guard, `no_client_driven_config_update_can_admit_the_agent`,
immediately failed with `the config-update application path` — its `.expect()` on
`source.find("fn apply_update(")`. The function had not moved. It had become
invisible.

`call_sites::scan::production_source` cuts every corpus at the **first column-0
`#[cfg(test)]`**, which is this repo's stated convention (a check whose own
patterns appear in its own file would otherwise match itself). Inserting a test
module mid-file moved that cut backwards past `apply_update`, so every
source-scanning check over `runtime/mod.rs` silently lost everything below the
new module — in this case, the exact function the guard exists to watch.

The guard failed loudly, which is why this cost twenty minutes rather than
shipping. Its own doc comment names the mutation it was built for and none of
them is "someone added a test above me".

## Lesson

**In a repo that cuts its scanned corpus at the first `#[cfg(test)]`, a test
module's position is production-code structure, not test organisation.** Put new
`#[cfg(test)]` modules at the end of the file, or immediately before the existing
first one — never between two pieces of production code.

The failure is not local to the module you add. It silently shortens the corpus
for *every* check that reads that file, including checks in other crates that
you have no reason to be thinking about.

## Why It Matters

This one failed loudly by luck: the guard used `.expect()` on its `find`. A check
written with `if source.contains(HAZARD)` instead would have gone **green** —
scanning a truncated corpus, finding no hazard, and reporting that the invariant
holds. That is LESSON-598's shape exactly ("a guard that has stopped covering its
subject looks exactly like a guard that passes"), reached by a route nobody would
look for: adding a test.

It also argues for the vacuity floors this repo already requires. A check that
asserts its corpus is at least N bytes, or that it found at least K call sites,
converts this from a silent hole into a failure with a readable message.

## Applies When

Adding a `#[cfg(test)]` module to any file a source-scanning check reads; writing
a new source-scanning check (give it a vacuity floor); debugging a structural
check that suddenly cannot find a function that plainly exists.

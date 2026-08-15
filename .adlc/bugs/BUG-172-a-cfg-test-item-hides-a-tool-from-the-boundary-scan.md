---
id: BUG-172
title: "A `#[cfg(test)]` item above an `impl Tool` block silently hides the tool from the boundary-coverage universe"
status: resolved
severity: medium
created: 2026-08-14
updated: 2026-08-14
component: "tests/boundary_coverage"
domain: "test-infrastructure"
stack: ["rust", "tests"]
tags: ["boundary-coverage", "req-571", "req-577", "lesson-432", "fail-safe-direction", "mutation-check"]
---

## Description

`boundary_coverage.rs` derives the universe of content-surfacing tools by
scanning each tool source's *production half* — everything before the file's
test code. `production_half` found that boundary by searching for the first
`"\n#[cfg(test)]\n"` line, i.e. the bare attribute. Its doc comment claimed the
failure mode was fail-safe: a file that loses its marker is scanned whole,
which can only make the universe *wider*.

That reasoning only covered a *lost* marker, not an *early* one. A single
`#[cfg(test)]` **item** — a const, a helper fn — declared above a file's
`impl Tool` block matches the marker first, truncates the scan before the
impl, and drops the tool from the derived universe. For a new tool with no
`COVERAGE` entry yet, both sides of the set comparison then agree by omission
and `every_content_surfacing_tool_has_a_boundary_test` passes with the tool
uncovered — the exact LESSON-432 shape the suite exists to stop. The
registry-derived second check only catches this for *registered* tools, so an
unregistered-but-implemented tool slips both nets.

This was hit for real during REQ-577 TASK-145: a `#[cfg(test)] const
MAX_DESCRIPTION_CHARS` at file scope in `harness/tools/docs.rs` hid `DocsTool`
from the scan. It was worked around by moving the constant into the test
module (the placement its doc comment then had to explain), so nothing was
broken on `main` — but the next file-scope `cfg(test)` item would reopen the
hole silently.

## Reproduction Steps

1. In any tool source under `crates/tetond/src/harness/tools/`, add
   `#[cfg(test)] const CEILING: usize = 1;` above the file's `impl Tool for …`
   block (on its own lines, as rustfmt writes it).
2. Delete that tool's `COVERAGE` entry — or, equivalently, imagine the tool is
   new and the entry was never written.
3. `cargo test -p tetond --test boundary_coverage` stays green: the tool is in
   neither the derived universe nor the claim.

## Expected Behavior

Only the test *module* ends the production half. Test-only items above the
impl never narrow the universe; a tool without coverage takes the suite red.

## Actual Behavior

The scan ended at the first `#[cfg(test)]` line of any kind, and the doc
comment asserted the wrong failure direction, so a reader had no reason to
distrust it.

## Environment

- Present since the suite landed with REQ-571; latent on `main` at 0.1.15
  (`4569311`). Nothing shipped is misclassified today — the defect is in what
  the suite would *fail to catch* next.

## Root Cause

The marker conflated the attribute with the thing it usually precedes. Every
tool file happens to spell its test boundary `#[cfg(test)]\nmod tests {`, so
anchoring on the attribute alone worked until a `cfg(test)` item appeared at
file scope — a perfectly ordinary Rust shape the marker had never met. The
"wider is fail-safe" argument was sound for the marker going *missing* and was
never re-examined for the marker matching *early*, which fails in the opposite
direction: the universe shrinks and the suite agrees with silence.

## Resolution

- **The anchor is now the attribute-plus-module pair**: `production_half` cuts
  at `"\n#[cfg(test)]\nmod "`. A lone `cfg(test)` item no longer matches; a
  file that loses the pair is scanned whole, which is genuinely the wider,
  fail-safe direction the doc comment now correctly describes.
- **A regression test pins the semantics**:
  `a_cfg_test_item_above_the_impl_does_not_hide_the_tool_from_the_scan` feeds
  the extractor a fixture with a `#[cfg(test)] const` above an impl and a
  test-module fake below it, asserting the impl survives and the fake stays
  excluded.
- **Mutation-checked, and the first fixture failed it** (LESSON-441 earning
  its keep): with the item at byte zero the old newline-anchored marker could
  never have matched it, so the test passed against the exact bug it targets.
  The fixture now opens with a module-doc line — the position every real
  file's items occupy — and carries a comment naming that line as
  load-bearing. Re-run: old marker fails with the silent-shrink message; new
  marker passes 6/6.
- **The REQ-577 workaround is no longer load-bearing**: the
  `MAX_DESCRIPTION_CHARS` comment in `docs.rs` now records its test-module
  placement as convention, not a constraint.

Residual, accepted: a `#[cfg(test)] mod helpers;` *module* above an impl would
still truncate early (rare; and it is genuinely test code), and the registry
check still covers only registered tools. Tighten the anchor to `mod tests`
if either ever bites.

## Deployment

n/a — test-only change; rides the next tagged release (post-0.1.15).

## Files Changed

- `crates/tetond/tests/boundary_coverage.rs` — the anchor, its doc comment,
  the regression test
- `crates/tetond/src/harness/tools/docs.rs` — the stale workaround note
- `.adlc/bugs/BUG-172-a-cfg-test-item-hides-a-tool-from-the-boundary-scan.md`
  — this file

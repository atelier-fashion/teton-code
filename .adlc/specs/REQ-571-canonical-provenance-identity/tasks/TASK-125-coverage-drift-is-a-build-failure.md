---
id: TASK-125
title: "Make boundary-coverage drift a build failure"
status: complete
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-120, TASK-122]
---

## Description

Implement BR-7 and BR-8 (ADR-E) — the anti-recurrence rules. Without these the
REQ fixes today's instance and leaves the mechanism that produced it intact:
LESSON-432's uncovered tools were exactly the vulnerable ones.

## Files to Create/Modify

- `crates/tetond/tests/boundary_coverage.rs` — new. The BR-7 enumeration (AC-12) and the BR-8 pairing (AC-13).
- `crates/teton-core/src/boundary.rs` — comments linking each retained matcher assertion to its tool-layer twin.

## Acceptance Criteria

- [x] AC-12: a test enumerates every tool that can surface external or file content — at minimum `read`, `edit`, `grep`, `glob`, `shell`, `web_fetch`, and MCP results — and asserts each has at least one boundary test.
- [x] AC-12: adding a content-surfacing tool without coverage **fails** this test. Demonstrate by adding a throwaway tool locally, observing red, then removing it; record the observation in the PR.
- [x] AC-13: each retained boundary-matcher assertion about absolute and `..`-bearing paths is paired with a tool-layer test proving that spelling cannot reach the matcher.
- [x] AC-13: the pairing is explicit — a shared fixture name or a comment naming the counterpart — so neither half can be deleted alone.
- [x] BR-8: the existing matcher assertions are RETAINED, not replaced. They are correct at the matcher layer.
- [x] AC-9 final sweep: `cargo test --workspace --no-fail-fast` green, all six egress suites unchanged.

## Technical Notes

Follow the precedent ADR-009 rule 3 already set — the marker-coverage test that
makes frame drift a build failure. The mechanism matters more than the current
tool list: a list that must be hand-updated is the guard LESSON-443 warns about.

Prefer deriving the tool list from the registry over hardcoding it, so a new
registration is picked up automatically. If the registry cannot be enumerated at
test time, hardcode it but assert the count against the registry so an addition
still trips the test.

Existing matcher assertions to pair: `crates/teton-core/src/boundary.rs:175-196`
(`out_of_repo_paths_never_match_and_never_panic`).

## As landed

`crates/tetond/tests/boundary_coverage.rs` (new, 5 tests) and 37 lines of
linking comment in `crates/teton-core/src/boundary.rs`. No other file changed.

**The tool list is derived twice, and only the coverage *claim* is written
down.** The universe comes (a) from a compile-time scan of every `impl Tool
for …` in the production half of the eight files under
`crates/tetond/src/harness/tools/`, and (b) from the live registry
(`ToolRegistry::with_builtins().names()`). `COVERAGE` — seven entries keyed by
impl type — is compared to each as a **set**, in both directions, so a tool
without an entry fails and an entry for a vanished type fails too. The scan is
`include_str!`, never a runtime read (BUG-159 / LESSON-441), and carries floors
(8 embedded files, each >500 bytes, `ReadTool`/`WebTool`/`McpToolHandle`
present) so an extractor that goes blind fails instead of agreeing with an
empty claim.

The fixed `include_str!` list is itself checked: `every_tool_source_file_is_scanned`
compares it against the `mod` declarations in `tools/mod.rs`, which a new tool
file must have to compile at all — so a tool in a *new* file is not a blind
spot.

A citation cannot be faked: the named test must exist in the named suite, must
carry a `#[test]`/`#[tokio::test]` attribute (a helper is not coverage), must
mention the tool it claims to cover, and may not be shared by two tools
(LESSON-502). There is deliberately **no exemption arm** — a future tool that
surfaces nothing has to be argued for in the file, in review.

**The AC-12 red was observed.** A throwaway `impl Tool for ThrowawayTool` added
to `read.rs` took `every_content_surfacing_tool_has_a_boundary_test` red with
`left: {…, "ThrowawayTool", …} right: {…}`; removing it restored green and
`read.rs` is byte-identical to HEAD. Ten further falsification legs were run
and each fired: dropping a file from the scan list, citing a nonexistent test,
citing a helper, an unclaimed registry name, a wrong `mention`, a shared
citation, deleting the twin-naming comment in `boundary.rs`, weakening a matcher
assertion, and renaming the matcher test away.

**AC-13 is deletion-proof from both ends.** The matcher assertions are retained
verbatim; each is preceded by a comment naming its tool-layer twins, and
`each_out_of_repo_matcher_assertion_is_paired_with_a_tool_layer_test` fails if
the matcher test goes, if either assertion is weakened, if the naming comment is
removed, or if a twin is renamed or deleted. `boundary.rs` says so in prose too,
naming the test that will fail.

One naming note: AC-12 says `web_fetch`; the tool's registered name is `web`
(`WEB_TOOL_NAME`), which the enumeration pins against the constant rather than a
literal.

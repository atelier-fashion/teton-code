---
id: TASK-125
title: "Make boundary-coverage drift a build failure"
status: draft
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

- [ ] AC-12: a test enumerates every tool that can surface external or file content — at minimum `read`, `edit`, `grep`, `glob`, `shell`, `web_fetch`, and MCP results — and asserts each has at least one boundary test.
- [ ] AC-12: adding a content-surfacing tool without coverage **fails** this test. Demonstrate by adding a throwaway tool locally, observing red, then removing it; record the observation in the PR.
- [ ] AC-13: each retained boundary-matcher assertion about absolute and `..`-bearing paths is paired with a tool-layer test proving that spelling cannot reach the matcher.
- [ ] AC-13: the pairing is explicit — a shared fixture name or a comment naming the counterpart — so neither half can be deleted alone.
- [ ] BR-8: the existing matcher assertions are RETAINED, not replaced. They are correct at the matcher layer.
- [ ] AC-9 final sweep: `cargo test --workspace --no-fail-fast` green, all six egress suites unchanged.

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

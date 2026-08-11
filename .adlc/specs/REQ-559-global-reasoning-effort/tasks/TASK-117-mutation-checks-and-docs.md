---
id: TASK-117
title: "AC-12 mutation checks, the full-suite sweep, and the architecture/context record"
status: pending
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-113, TASK-116]
---

## Description

The closing task. AC-12 asks for a mutation check proving two specific
weakenings each turn a test red: removing the always-send rule (BR-1), and making
the clamp an identity function. It also records the two reusable decisions
(ADR-A's shape-as-return-type and ADR-E's intersection default) in
`.adlc/context/architecture.md` so the next REQ inherits them.

## Files to Create/Modify

- `crates/teton-core/src/effort.rs` — any test-visibility hooks the mutation
  check needs (behind `#[cfg(test)]`)
- `crates/tetond/tests/` — the integration sweep
- `.adlc/context/architecture.md` — the two Key Patterns entries

## Acceptance Criteria

- [ ] **AC-12 mutation A**: removing the always-send rule — making
      `resolve_effort` return `Omit(ShapeNone)` for an undeclared remote provider
      instead of the ADR-E `effort_only` default — turns at least one test red.
      Name the test in the mutation note.
- [ ] **AC-12 mutation B**: making `EffortLadder::clamp` an identity function
      (returning `Some(requested)` regardless of the ladder) turns at least one
      test red. AC-3's table must be the thing that catches it; if it is not,
      AC-3's ladders are too close to the full canonical set to discriminate.
- [ ] Each mutation is applied, the failing test names are recorded, and the
      mutation is reverted. The record goes in the PR body, not in a committed
      file (LESSON-441: a fix pass is new code — re-verify adversarially, not by
      test count).
- [ ] `cargo test --workspace --no-fail-fast` is green. **`--no-fail-fast` is
      mandatory** — this repo's fail-fast run hides whole targets, so a reported
      failure count from a default run is a floor, not a total.
- [ ] `cargo clippy --workspace --all-targets` produces no new warnings.
- [ ] `.adlc/context/architecture.md` gains two Key Patterns entries:
      - *A wire shape is a return type, not a pair of flags* — when two request
        fields are mutually exclusive because a provider 400s on both, encode the
        exclusion as an enum whose variants are the outcomes, so the illegal state
        is unrepresentable rather than merely tested (ADR-A).
      - *An unknown endpoint's default capability is the intersection, not the
        superset* — a permissive default for a provider you cannot identify sends
        values it will reject, and the rejection path lands back on the vendor
        default the feature existed to override (ADR-E).
- [ ] The REQ's remaining open questions (OQ-1's flag ergonomics, OQ-4's
      per-category breakout) are carried into the requirement's Open Questions
      with their current status rather than silently dropped.

## Technical Notes

**BUG-159 hazard — read before running the mutation check.** `call_sites.rs` and
`harness/duty.rs` read production source with `.expect("readable source file")`
after walking it, so any writer touching `src/` mid-run panics those five tests.
The AC-12 mutation check is exactly that pattern: it edits `src/`, runs the
suite, and reverts. **Do not run the mutation check concurrently with any other
edit**, and if you see that panic, it is BUG-159, not a regression from this REQ.
Run the mutations serially, one at a time, with no other work in flight.

**Do not weaken a test to make a mutation "detectable".** If mutation B is not
caught, the fix is a narrower ladder in AC-3's table (one that actually
discriminates), not a new assertion bolted on beside it. A mutation check that
passes only because a test was written to catch that exact mutation proves
nothing about the next one.

**The workspace build must be current before targeted runs.** A targeted
`-p teton --test ...` run does not rebuild `tetond`, so a mutation can look
survived when it was not. Build the workspace first.

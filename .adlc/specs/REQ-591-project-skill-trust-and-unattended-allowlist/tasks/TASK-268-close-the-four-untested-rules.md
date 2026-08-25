---
id: TASK-268
title: "Close the four rules that have never had a test"
status: draft
parent: REQ-591
created: 2026-08-25
updated: 2026-08-25
dependencies: [TASK-264]
---

## Description

**Found in Phase 3 validation.** AC-12 through AC-15 are not satisfied by the code being carved
out — they were added to the spec during `/validate` precisely *because* BR-6, BR-7, BR-11 and
BR-12 had no coverage. The carve-out moves code; it does not write the missing tests, and
letting these ride along uncovered would repeat the failure `/validate` just caught.

Each is small. Together they are the difference between shipping four documented rules and
shipping four documented rules that are actually checked.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/skill.rs` — the exact-match test (AC-12)
- `crates/tetond/src/harness/permissions.rs` — label/effect pinning (AC-13)
- `crates/teton-protocol/src/events.rs` — the `ProjectSkillTrust::root` contract (AC-14)
- `crates/tetond/src/harness/docs/skills.md` — the documentation (AC-15)
- `docs/manual-verification.md` — the dogfood runbook (AC-11)

## Acceptance Criteria

- [ ] **AC-12 (BR-6)**: trusting `~/dev/repo` does NOT authorize `~/dev/repo/vendor/other`.
      Paired with a positive leg on the same fixture so neither passes by accident. A prefix
      match would let a dependency update place a tree inside a listed root and inherit its
      trust — this rule was written and never checked.
- [ ] **AC-13 (BR-7)**: the option label and the write it performs are pinned by ONE test, so
      they cannot drift. LESSON-495's recorded failure is a prompt describing a write that
      provably could not happen.
- [ ] **AC-14 (BR-11)**: a directory name containing a newline or ESC does not reach a client
      raw. Either bound and control-strip at the minting door with a test, or correct the wire
      contract — but not the present state, where the doc claims a bounding that does not happen.
- [ ] **AC-15 (BR-12)**: `skills.md` documents the acknowledgment and `trusted_project_roots`,
      and the existing byte-ceiling test still passes. **Measure headroom first** — the file is
      4,092 bytes against a 4,096 ceiling — and state in the commit what was cut to pay for it.
- [ ] **AC-11**: the dogfood runbook is written into `docs/manual-verification.md`, covering a
      listed root, an unlisted root, and a daemon whose `$HOME` differs from its launch
      environment (OQ-4's case).
- [ ] `cargo test --workspace --no-fail-fast` green.

## Technical Notes

TASK-258 in REQ-589 hit the same `skills.md` ceiling and paid for its sentence by shortening two
unpinned lines — read that commit (`31d7f15`) before reaching for the ceiling itself.

AC-14 is a judgment call, not a mechanical fix: bounding the string daemon-side changes what
existing clients render, while correcting the contract leaves a third-party client to defuse.
Say which you chose and why.

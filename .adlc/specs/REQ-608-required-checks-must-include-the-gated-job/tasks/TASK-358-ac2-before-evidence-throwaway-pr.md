---
id: TASK-358
title: "Demonstrate the defect on the forge: a red gated job is mergeable under current protection (AC-2 before-half)"
status: complete
parent: REQ-608
created: 2026-09-02
updated: 2026-09-02
dependencies: []
repo: teton-code
---

## Description

Open a throwaway PR whose only change makes the `gated` job fail, wait for CI
to settle, and record the forge's own mergeability verdict (ADR-608-7). This is
the "before" half of AC-2 and is required unconditionally by the spec. The
branch and PR are created from `origin/main`, not from this REQ's branch, so
the evidence is about today's protection and nothing else.

## Files to Create/Modify

- `crates/tetond/tests/template_smoke.rs` — on the throwaway branch
  `chore/REQ-608-ac2-evidence` ONLY (never on the REQ branch): add one line
  `compile_error!("REQ-608 AC-2 evidence: deliberate all-features break");`
  inside the existing `#![cfg(feature = "llama")]` target.
- `.adlc/specs/REQ-608-required-checks-must-include-the-gated-job/requirement.md`
  — on the REQ branch: under AC-2, record the throwaway PR number, the head SHA,
  the failing job name, and the verbatim `mergeable` / `mergeStateStatus` pair.

## Acceptance Criteria

- [ ] The throwaway branch is cut from `origin/main` and its single commit
      touches only `template_smoke.rs`.
- [ ] The PR is non-draft (a draft reports `DRAFT` and hides the verdict), titled
      `[REQ-608 AC-2 evidence — DO NOT MERGE] deliberate gated-job failure`, body
      says why it exists and that it will be closed unmerged.
- [ ] CI on the PR: `feature-gated targets compile (all features)` is `failure`;
      every other job is `success` (if any other job is red the evidence is
      confounded — fix or wait, do not record).
- [ ] `gh pr view <n> --json mergeable,mergeStateStatus` reports
      `MERGEABLE` / `UNSTABLE` — recorded verbatim with the timestamp. Any other
      pair is recorded verbatim too and the discrepancy is surfaced; the claim
      is whatever the forge says.
- [ ] `mergeable` is polled alongside checks (LESSON-461): a `CONFLICTING` PR
      produces no runs and must not be read as "checks pending".
- [ ] The PR stays open for TASK-359's "after" half; it is closed and its branch
      deleted by TASK-359, not here.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-2 | structural-check | `requirement.md` AC-2: throwaway PR number, head SHA, and verbatim `mergeable`/`mergeStateStatus` before the edit | no |

## Technical Notes

- Work in a separate worktree (`git worktree add <path> -b chore/REQ-608-ac2-evidence origin/main`)
  so the REQ worktree is untouched; remove it when done.
- Wait loop: `gh pr checks <n> --watch` can exit early on a conflicted PR;
  poll `gh pr view --json mergeable,mergeStateStatus,statusCheckRollup` every
  60 s until every rollup entry is terminal. The macOS `gated` leg and the
  macOS `check` leg dominate (~10–15 min).
- Nothing here needs admin rights. Nothing is merged.

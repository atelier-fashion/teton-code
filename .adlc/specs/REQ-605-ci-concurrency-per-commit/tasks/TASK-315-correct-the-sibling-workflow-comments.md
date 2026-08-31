---
id: TASK-315
title: "Correct the two sibling-workflow comments that define themselves against ci.yml"
status: draft
parent: REQ-605
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-314]
repo: teton-code
---

## Description

`release.yml` and `deploy-site.yml` both explain their `cancel-in-progress: false`
by contrast: "**Unlike ci.yml**, an in-flight run is NOT cancelled by a newer
one." TASK-314 makes `ci.yml` stop cancelling on a newer push, so both sentences
become false on the commit that lands it.

The distinction between these workflows and `ci.yml` is still real — it has
moved. `release.yml` and `deploy-site.yml` **queue** same-group runs, because a
half-published release or half-rolled revision is worse than waiting. `ci.yml`
runs distinct commits **concurrently** and cancels only same-commit duplicates.
Rewrite both comments to say that.

This is the compiler-invisible half of the change (LESSON-599): nothing fails if
it is skipped, and the next reader is misled.

## Files to Create/Modify

- `.github/workflows/release.yml` — the comment at line 22 above the `concurrency:` block
- `.github/workflows/deploy-site.yml` — the comment at line 60 above the `concurrency:` block

## Acceptance Criteria

- [ ] Neither file claims `ci.yml` cancels an in-flight run on a newer push
- [ ] Each comment still explains **its own** reason for `cancel-in-progress: false`
      (a half-published release / a half-synced bucket or half-rolled revision) —
      the existing rationale is correct and must survive the edit
- [ ] Each comment states the *new* contrast: these workflows queue, `ci.yml`
      runs distinct commits concurrently
- [ ] No behavioural key changes in either file — comments only. `git diff` for
      both files shows no non-comment line
- [ ] `actionlint -color -shellcheck shellcheck .github/workflows/*.yml` passes

## Technical Notes

`grep -rn "ci\.yml" .github/ docs/ tools/` finds three further references. All
three were checked and are unaffected — leave them alone:

- `release.yml:218` — action major-tag pinning
- `docs/release-runbook.md:405` — the `tooling` job's existence
- `tools/release/verify-version.sh:23` — the exit-code taxonomy

Comment-only edits, so the check that matters is the prose diff, not the test
suite: `git diff origin/main..HEAD -- .github/workflows/release.yml .github/workflows/deploy-site.yml`
should be entirely `#`-prefixed lines.

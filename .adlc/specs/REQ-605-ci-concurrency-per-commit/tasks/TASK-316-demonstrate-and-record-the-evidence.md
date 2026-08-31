---
id: TASK-316
title: "Demonstrate the property on a real push sequence and record the evidence"
status: complete
parent: REQ-605
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-314, TASK-315]
repo: teton-code
---

## Description

AC-1 is not satisfied by the diff. It requires the property observed on a real
sequence of pushes, with run ids and conclusions recorded; AC-2 requires the
before/after runner-minutes under a named counting rule. This task performs the
observation and writes both into the requirement's verification section.

REQ-599's AC-11 and REQ-600's AC-2 are both in this repo's history because a
criterion of exactly this shape was ticked from weaker evidence. If the
observation cannot be made as specified, AC-1 is recorded **NOT MET with the
reason** — not ticked from a substitute.

## Files to Create/Modify

- `.adlc/specs/REQ-605-ci-concurrency-per-commit/requirement.md` — a Verification section carrying the run table, the AC-2 figures and their counting rule, and the AC status marks

## Acceptance Criteria

- [ ] At least **two** consecutive pushes are observed under the **new** config
      where the earlier run was **in flight** when the later was pushed, and the
      earlier run still reached a terminal conclusion that is not `cancelled`
- [ ] At least **one** pair is observed under the **old** config showing the
      earlier run `cancelled` — the before half. Historical runs may corroborate
      but do not substitute for an observation on this branch
- [ ] Every recorded run states **which configuration it ran under**. A run
      whose commit predates the `ci.yml` change is labelled old; one at or after
      it is labelled new. Mixed pairs (old run, new run) are recorded as mixed
      and are **not** counted as evidence for either half
- [ ] In-flight status is **verified before each push** (`gh run view <id>
      --json status`), not inferred from timing. An observation where the earlier
      run had already completed is vacuous and must be discarded and redone
- [ ] AC-2's before/after figures state Rule R and Rule W verbatim, note that
      `timing.billable` reads 0 on this public repo and that job timestamps are
      the source, and give both the Before-A (push freely) and Before-B (wait for
      each) baselines
- [ ] Every AC in the requirement carries a mark backed by named evidence — a run
      id, a command, or a file — and any unmet AC says so plainly

## Technical Notes

Run ids and conclusions come from `gh run list --workflow=ci.yml --branch
feat/REQ-605-ci-concurrency-per-commit` and `gh run view <id>`. Per-job detail
(which job was cancelled, and where) comes from
`/repos/{owner}/{repo}/actions/runs/{id}/jobs`; the `conclusion` on the **job**
is what distinguishes a cancelled macOS leg from a cancelled run.

**`timing.billable` is not the source.** It returns `total_ms: 0` for every job
on this public repo. Job `started_at`/`completed_at` are.

**LESSON-461.** If an expected run never appears, check `gh pr view --json
mergeable,mergeStateStatus` before retriggering. A `CONFLICTING` PR has no merge
ref and produces no `pull_request` runs at all — absence, not failure. That
silence would otherwise read as "the change broke CI".

The last commit in the sequence cannot record its own run's conclusion. Record
that run's id and state that its conclusion is confirmed before merge, rather
than leaving the table looking complete when it is not.

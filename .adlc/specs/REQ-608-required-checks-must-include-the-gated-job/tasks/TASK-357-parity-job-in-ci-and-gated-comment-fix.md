---
id: TASK-357
title: "Add the parity job to ci.yml, fix the gated comment to cite BUG-167, and record the job-change runbook"
status: draft
parent: REQ-608
created: 2026-09-02
updated: 2026-09-02
dependencies: [TASK-356]
repo: teton-code
---

## Description

Wire the check from TASK-356 into `.github/workflows/ci.yml` as its own job
(ADR-608-1), correct the `gated` job's comment (BR-8), and write the
add-or-rename-a-job runbook into `conventions.md` (ADR-608-8). Lint locally with
the same actionlint version CI pins.

## Files to Create/Modify

- `.github/workflows/ci.yml` — (a) line ~80: `BUG-166` → `BUG-167`, and re-check
  the sentence against BUG-167's description (it says the REQ-554 AC-6 weights
  smoke `template_smoke.rs` sat broken through REQ-564's constructor change —
  that claim is accurate; keep it, cite BUG-167). (b) New job `parity`,
  `name: required checks mirror ci.yml (REQ-608)`, `runs-on: ubuntu-latest`, no
  `if:`, inheriting the workflow-level `permissions: contents: read` (no
  override). Steps: checkout (`actions/checkout@v4`, matching the other jobs),
  guarded `PyYAML==6.0.2` install, unit tests with a non-vacuous assertion, then
  the live check with `GITHUB_TOKEN` in env and a `case` on 0/1/75 rendering
  `::notice::`/`::error::` with titles, in the shape of the `catalog` job. A
  header comment explains why the job exists, why it is its own job, and why
  no permission widens (BR-10).
- `.adlc/context/conventions.md` — under "Git Conventions", one paragraph:
  every job `ci.yml` defines is a required check on `main`, the parity job
  enforces that in both directions, and the sequence for adding or renaming a
  job (ADR-608-8 runbook).

## Acceptance Criteria

- [ ] `actionlint -color -shellcheck shellcheck .github/workflows/*.yml` passes
      locally with actionlint 1.7.12 (the CI pin).
- [ ] `grep -n BUG-166 .github/workflows/ci.yml` returns nothing; the `gated`
      comment cites BUG-167 and its `template_smoke.rs` claim matches BUG-167's
      description (AC-8).
- [ ] The new job has no `if:`, no `paths`, and no `permissions:` block;
      `permissions:` at workflow level is unchanged (`contents: read`) (AC-9: the
      before/after effective-permission diff is "none" for every job — stated in
      the job's header comment).
- [ ] The job's unit-test step fails if `Ran 0 tests` is reported (vacuous-run
      guard).
- [ ] The job's live-check step branches on 0 / 1 / 75 and each branch prints a
      titled annotation; any other code is treated as failure.
- [ ] `python3 tools/ci/required-checks-parity.py --workflow .github/workflows/ci.yml --repo atelier-fashion/teton-code`
      run locally (before the admin edit) shows the new job's own name under
      `missing:` — the check sees itself.
- [ ] `conventions.md` gains the runbook paragraph.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | structural-check | `.github/workflows/ci.yml`: `gated` job has no `if:`/paths (grep), and TASK-359's protection PATCH names its context | no |
| BR-7 | structural-check | `.github/workflows/ci.yml`: `gated` and `parity` carry no `if:`, `on.pull_request` carries no `paths` (grep asserted in TASK-359's evidence) | yes |
| BR-8 | structural-check | `.github/workflows/ci.yml`: `grep -c BUG-166` is 0, `gated` comment cites BUG-167 | no |
| BR-10 | structural-check | `.github/workflows/ci.yml`: workflow `permissions:` is `contents: read` and no job declares `permissions:` (grep) | no |
| AC-7 | structural-check | `.github/workflows/ci.yml`: `gated` has no `if:`; `on.pull_request` has no `paths` (quoted in TASK-359's evidence) | yes |
| AC-8 | structural-check | `.github/workflows/ci.yml`: `gated` comment cites BUG-167 | no |
| AC-9 | structural-check | `.github/workflows/ci.yml`: no job-level `permissions:` (grep) | no |
| AC-11 | structural-check | `actionlint -shellcheck shellcheck .github/workflows/*.yml` exit 0 over 3 files | no |

## Technical Notes

- Keep `actions/checkout@v4` unpinned to match `ci.yml`'s other jobs (SHA-pins
  are the credential-bearing workflows' rule, not this one's).
- `set -euo pipefail` in every `run:` block; use `set +e` / `set -e` around the
  live check the way the `catalog` job does so the exit code can be captured.
- The pip install step:
  ```sh
  if python3 -c 'import yaml' 2>/dev/null; then echo "PyYAML present"; else python3 -m pip install --user 'PyYAML==6.0.2'; fi
  python3 -c 'import yaml; print("PyYAML", yaml.__version__)'
  ```
  A failed install fails the step — LESSON-447.
- Unit-test step: run `python3 -m unittest tools/ci/test_required_checks_parity.py -v 2>&1 | tee /tmp/parity-tests.log`
  is NOT safe (pipefail + tee semantics); instead capture to a file, then grep
  `Ran [1-9][0-9]* tests?` and fail with `::error::` if absent.
- Pass `GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}` via `env:` on the step, and
  `GITHUB_REPOSITORY` is already in the runner env.
- `crates/teton-inference/tests/catalog_integrity.rs` asserts `ci.yml` contains
  `refresh-catalog.py`, `--check`, and `75` — untouched by this change; confirm
  with `cargo test -p teton-inference --test catalog_integrity`.

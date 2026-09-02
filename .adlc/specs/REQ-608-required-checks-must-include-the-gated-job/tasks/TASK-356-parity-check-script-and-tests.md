---
id: TASK-356
title: "Write the required-checks parity script and its known-bad unit tests"
status: draft
parent: REQ-608
created: 2026-09-02
updated: 2026-09-02
dependencies: []
repo: teton-code
---

## Description

Create `tools/ci/required-checks-parity.py`: parse `.github/workflows/ci.yml`
into the set of check-run contexts it defines (ADR-608-4), read `main`'s
required contexts from the forge (ADR-608-2), compare in both directions, and
exit 0 / 1 / 75 per ADR-608-3 with output that names both BR-9 remedies. Create
`tools/ci/test_required_checks_parity.py` with the mutation table from
ADR-608-6, injected fakes only, asserting on rendered output.

## Files to Create/Modify

- `tools/ci/required-checks-parity.py` — the check. Python 3.9-compatible, stdlib
  plus PyYAML (imported inside `main()`, missing → 75 with the pip remedy).
  Functions: `derive_contexts(workflow: dict) -> list[str]` (raises
  `Underivable(job_key, reason)`), `read_required(fetch, owner_repo, branch)`
  (raises `Unverified(reason)`), `compare(defined, required) -> (missing, stale)`,
  `render(...)`, `main(argv) -> int`. CLI: `--workflow PATH` (default
  `.github/workflows/ci.yml`), `--repo OWNER/REPO` (default `$GITHUB_REPOSITORY`),
  `--branch NAME` (default `main`), token from `$GITHUB_TOKEN` (optional).
- `tools/ci/test_required_checks_parity.py` — `unittest` cases; every row of the
  ADR-608-6 mutation table, each with a doc comment naming the AC it discharges
  and what went red when the mutation was first run.

## Acceptance Criteria

- [ ] `python3 tools/ci/required-checks-parity.py --repo atelier-fashion/teton-code`
      run locally against the live `main` (before the admin edit) exits 1 and its
      output lists `feature-gated targets compile (all features)` and
      `required checks mirror ci.yml (REQ-608)` under `missing:`, nothing under
      `stale:`, and both remedies verbatim: "revert the protection edit" and
      "update .github/workflows/ci.yml".
- [ ] `derive_contexts` on the real `ci.yml` yields exactly the seven contexts
      today plus the new job's once TASK-357 lands; the matrix leg expands to
      `fmt · clippy · test (ubuntu-latest)` and `fmt · clippy · test (macos-latest)`.
- [ ] Every mutation row in ADR-608-6 is a test case; the benign row asserts
      exit 0 and both sets rendered.
- [ ] A fake fetch returning 401 → exit 75; output contains `401` and the URL.
      A fake fetch raising `OSError` → exit 75; output names `OSError`. An
      unforeseen exception inside `main` → 75, never 1 (test by injecting a fetch
      that raises `RuntimeError`).
- [ ] `protected: false` → 75 "not protected". Non-empty rulesets list → 75
      naming rulesets. Two-dimension matrix → 75 naming the job key.
- [ ] Output for both drift directions contains both remedies (AC-10 asserted on
      rendered text).
- [ ] `python3 -m unittest tools/ci/test_required_checks_parity.py -v` passes on
      Python 3.9 locally; no test opens a socket (fetch is always injected).
- [ ] The script's header comment states the derivation rule, the exit taxonomy,
      and cites REQ-608, LESSON-442, LESSON-464.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `tools/ci/test_required_checks_parity.py::test_missing_job_fails` | yes |
| BR-3 | test-case | `tools/ci/test_required_checks_parity.py::test_matrix_expands_single_dimension` | no |
| BR-3 | test-case | `tools/ci/test_required_checks_parity.py::test_multi_dimension_matrix_is_underivable` | no |
| BR-4 | test-case | `tools/ci/test_required_checks_parity.py::test_stale_context_fails` | yes |
| BR-5 | test-case | `tools/ci/test_required_checks_parity.py::test_read_401_fails_closed` | yes |
| BR-5 | test-case | `tools/ci/test_required_checks_parity.py::test_unprotected_branch_fails_closed` | no |
| BR-5 | test-case | `tools/ci/test_required_checks_parity.py::test_rulesets_present_fails_closed` | no |
| BR-6 | test-case | `tools/ci/test_required_checks_parity.py::test_deleting_a_required_context_goes_red` | yes |
| BR-9 | test-case | `tools/ci/test_required_checks_parity.py::test_both_directions_name_both_remedies` | yes |
| AC-3 | test-case | `tools/ci/test_required_checks_parity.py::test_both_directions_name_both_remedies` | yes |
| AC-4 | test-case | `tools/ci/test_required_checks_parity.py::test_added_job_in_fixture_goes_red` | yes |
| AC-5 | test-case | `tools/ci/test_required_checks_parity.py::test_read_401_fails_closed` | yes |
| AC-6 | test-case | `tools/ci/test_required_checks_parity.py::test_deleting_a_required_context_goes_red` | yes |
| AC-10 | test-case | `tools/ci/test_required_checks_parity.py::test_both_directions_name_both_remedies` | yes |

## Technical Notes

- Mirror `tools/refresh-catalog.py`: stdlib `urllib.request`, explicit timeout
  (20 s), `User-Agent` and `Accept: application/vnd.github+json` headers,
  `X-GitHub-Api-Version: 2022-11-28`. Catch by behaviour class (`OSError`,
  `http.client.HTTPException`, `json.JSONDecodeError`, `ValueError`) — LESSON-442.
- Required set: `body["protection"]["required_status_checks"]["contexts"]`.
  Also read `GET /repos/{o}/{r}/rules/branches/{b}`; a non-empty list → 75.
- Render sets sorted, one per line, prefixed `  - `. Headers exactly
  `defined by ci.yml:`, `required by main:`, `missing (defined, not required):`,
  `stale (required, not defined):`. Remedies block, verbatim in both failure
  directions:
  ```
  Two ways to resolve this, pick the one that matches intent:
    1. revert the protection edit — restore main's required checks to the set ci.yml defines
    2. update .github/workflows/ci.yml — make the defined jobs match the intended required set
  (main's required checks are edited by a repository admin under Settings > Branches; never by a workflow)
  ```
- Emit `::warning title=...::` for any job with `if:` or a `paths` filter on
  `on.pull_request` (ADR-608-4); do not change the exit code for it.
- Tests: build fixture workflows as dicts (or small YAML strings loaded with
  `yaml.safe_load`); load the real `ci.yml` for the benign path and the matrix
  case. Use `contextlib.redirect_stdout` to capture rendered output. Never call
  the real fetcher.
- Keep the file executable-free (invoked as `python3 tools/ci/...`), matching
  `refresh-catalog.py`.

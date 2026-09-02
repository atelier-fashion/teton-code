---
id: TASK-359
title: "Require gated and parity on main (admin edit), record before/after, capture the AC-2 after-half, close the throwaway PR"
status: draft
parent: REQ-608
created: 2026-09-02
updated: 2026-09-02
dependencies: [TASK-357, TASK-358]
repo: teton-code
---

## Description

The one step a workflow cannot perform. Prepare the exact protection edit,
present it to the repository admin with the before state and the checks that
make it safe (BR-7), and — only on the admin's explicit go-ahead — apply it.
Then record the after state, capture AC-2's "after" verdict on the throwaway PR
from TASK-358, close that PR, and delete its branch. Sequence per ADR-608-8.

## Files to Create/Modify

- `.adlc/specs/REQ-608-required-checks-must-include-the-gated-job/requirement.md`
  — AC-1: the required-contexts list before and after (contexts only, not the
  full payload — LESSON-462). AC-2: the after-half verdict verbatim. AC-7: the
  quoted `gated` trigger lines and the `on.pull_request` block. AC-9: the
  per-job effective-permissions statement (unchanged for all eight).
- `docs/release-runbook.md` — no change expected; confirm it does not enumerate
  required checks (mapper found none). If it does, update.

## Acceptance Criteria

- [ ] Pre-conditions confirmed and quoted in the evidence before the edit: `gated`
      green on `main` (latest run id), `gated` has no `if:`/`paths`, PR #271's
      `ci.yml` defines both contexts, the AC-2 before-half is recorded.
- [ ] The edit is `PATCH /repos/atelier-fashion/teton-code/branches/main/protection/required_status_checks`
      with `contexts` = the six existing + `feature-gated targets compile (all features)`
      + `required checks mirror ci.yml (REQ-608)`, `strict` unchanged (`true`).
      It is applied by the repository admin, or by this pipeline with the
      admin's explicit consent recorded in the task file — never silently.
- [ ] After the edit: `GET .../branches/main/protection` lists eight contexts;
      recorded under AC-1 with a timestamp.
- [ ] PR #271's parity job is green after the edit (re-run if it ran before the
      edit) and `gh pr view 271 --json mergeStateStatus` is not `BLOCKED` on the
      parity or gated contexts.
- [ ] Throwaway PR from TASK-358: `gh pr view <n> --json mergeable,mergeStateStatus`
      now reports `BLOCKED` — recorded verbatim under AC-2 (after-half). Then the
      PR is closed with a comment naming REQ-608 and the branch is deleted.
- [ ] `enforce_admins` was `false` before and is left as it was — noted, not
      changed (OQ-4 territory).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | structural-check | `requirement.md` AC-1: after-list contains the `gated` context; live `GET .../branches/main/protection` | no |
| BR-7 | structural-check | `requirement.md` AC-7: quoted trigger lines; `gated` green on the recorded run | yes |
| BR-10 | structural-check | `requirement.md` AC-9: per-job permissions before/after statement | no |
| AC-1 | structural-check | `requirement.md` AC-1: before (6) and after (8) context lists with timestamps | no |
| AC-2 | structural-check | `requirement.md` AC-2: after-half `BLOCKED` verbatim on the throwaway PR | no |
| AC-7 | structural-check | `requirement.md` AC-7: quoted `gated` and `on.pull_request` lines | yes |
| AC-9 | structural-check | `requirement.md` AC-9: effective-permissions diff per job | no |

## Technical Notes

- The `gh` session in use is the admin's own (`repo` scope; the `GET
  .../protection` already succeeded). Applying the PATCH through it is the admin
  acting through the pipeline, not a bot token — but it is still a repository
  setting, so the pipeline **asks first** and records the answer here.
- Command to present:
  ```sh
  gh api -X PATCH repos/atelier-fashion/teton-code/branches/main/protection/required_status_checks \
    --input - <<'JSON'
  {"strict": true, "contexts": [
    "catalog integrity (BR-8/AC-8)",
    "fmt · clippy · test (ubuntu-latest)",
    "fmt · clippy · test (macos-latest)",
    "acceptance suite (REQ-544 + REQ-547)",
    "dependency advisories (cargo audit)",
    "release tooling (actionlint · shellcheck · selftest)",
    "feature-gated targets compile (all features)",
    "required checks mirror ci.yml (REQ-608)"
  ]}
  JSON
  ```
- Between the edit and PR #271's merge, any other PR would wait on the parity
  context; the manifest at Step 0 showed none in flight. Say so in the evidence.
- If the admin declines: record that, leave protection unchanged, mark AC-1 and
  the AC-2 after-half as not done with the reason, and let the pipeline finish
  as `pr-ready` rather than `merged` — the check in the tree is still shipped
  and will report `missing` until the edit lands.

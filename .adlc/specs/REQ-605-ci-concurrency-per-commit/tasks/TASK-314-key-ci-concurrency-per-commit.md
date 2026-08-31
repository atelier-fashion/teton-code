---
id: TASK-314
title: "Key the CI concurrency group on the commit under test"
status: draft
parent: REQ-605
created: 2026-08-31
updated: 2026-08-31
dependencies: []
repo: teton-code
---

## Description

Change `ci.yml`'s concurrency group from ref-only to ref-plus-commit, so pushing
commit *n+1* cannot cancel commit *n*'s in-flight run. Keep
`cancel-in-progress: true` — with a per-commit group it still collapses duplicate
runs of the *same* commit (a re-run, a close/reopen), which is the only thing it
can still usefully do.

Rewrite the block's comment. The existing one ("One in-flight run per ref; newer
pushes cancel older ones.") states the behaviour being removed, and a comment
that describes the old rule is worse than no comment.

## Files to Create/Modify

- `.github/workflows/ci.yml` — the `concurrency:` block (lines 9-12) and its comment

## Acceptance Criteria

- [ ] `group:` is `ci-${{ github.ref }}-${{ github.sha }}`
- [ ] `cancel-in-progress:` remains `true`
- [ ] The comment states the new rule, names what `cancel-in-progress` still
      collapses, and names the trade (a force-pushed-away commit's run is no
      longer killed) — a reader must not have to consult the REQ to learn why
- [ ] `actionlint` passes locally over `.github/workflows/*.yml` (AC-4), run at
      the **same pinned version CI uses** — `ACTIONLINT_VERSION: 1.7.12` in the
      `tooling` job — and shellcheck-backed, matching CI's invocation
- [ ] No other key in `ci.yml` changes: the trigger set, the seven job runs and
      the matrix are untouched (out of scope per the requirement)

## Technical Notes

The expression is `${{ github.sha }}`, not `${{ github.event.pull_request.head.sha }}`
— see ADR-605-1. One expression covers both triggers; on a `pull_request` event
`github.sha` is the synthetic merge commit and on a `push` event it is the pushed
commit, and both are unique per tree under test, which is the only property the
key needs.

Verify with the same command the `tooling` job runs, so a local pass means the
same thing CI's pass means:

```
actionlint -color -shellcheck shellcheck .github/workflows/*.yml
```

Do not reformat the file or touch neighbouring blocks — the diff should be the
concurrency block and its comment, nothing else.

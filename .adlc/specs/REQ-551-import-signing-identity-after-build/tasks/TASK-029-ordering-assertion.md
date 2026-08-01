---
id: TASK-029
title: "Mechanical import-after-build ordering assertion (BR-6)"
status: complete
parent: REQ-551
created: 2026-08-01
updated: 2026-08-01
dependencies: [TASK-028]
---

## Description

ADR-551-2: a selftest case that parses release.yml's build-job step
sequence and fails whenever the signing-identity import does not sit
strictly between the build invocation and the pack invocation — the
guard that outlives comments (BR-6, AC-4).

## Files to Create/Modify

- `tools/release/selftest.sh` — new case group beside the team-id consistency case: extract the build job's step names/run-lines in order (awk over release.yml); assert index(`package.sh … build`) < index("Import the Developer ID signing identity") < index(`package.sh … pack`); a missing marker (renamed step, removed phase arg) is a FAILURE naming what vanished, never a silent pass (the assertion must not be satisfiable by deleting its anchors — LESSON-443)

- `.github/workflows/release.yml` — scope extension (recorded 2026-08-01, from TASK-028 concern #1): the import step's error annotations still claim "Nothing was built." and "auto-lock mid-build" — false since the reorder; update those annotation strings (and only those) to state the true cost (the build is already done; a failure here loses seconds of signing setup, and the leg fails before signing). Also add the pointer comment naming the selftest ordering assertion once it exists (deferred from TASK-028 per LESSON-443)

## Acceptance Criteria

- [x] Case fails when the import step is moved above the build step in a scratch copy (AC-4 mutation, performed and recorded in the case comment) and when either anchor string is absent — the mutation is a PERMANENT case rather than a one-time manual proof: `import_order_check` is a function taking a workflow path, graded against `$RELEASE_WORKFLOW` for the real cases and against four known-bad scratch copies (import moved above the build; step renamed; each phase argument dropped) on every run
- [x] Runs in ci.yml's tooling job via the existing selftest invocation — no CI changes needed (verified by running the suite)
- [x] shellcheck clean; suite fully green; new total 323/323 (was 310, +13)

## Technical Notes

Mirror the cross-file consistency case's file-reading pattern
(selftest.sh ~1876-1895). Anchor on the step NAME for import (TASK-028
keeps it verbatim) and on the `package.sh` phase-argument invocations for
build/pack — these are the load-bearing strings; the case comment must say
so, so a future rename updates the assertion in the same commit.

Recorded at implementation: `Nothing was built.` survives in three places
outside the scope of this task — release.yml ~67, ~111 and ~128, all in the
`preflight` job, where it is still TRUE (preflight gates before the `build`
job runs at all). Only the import step's 16 occurrences became false when the
step moved below the build, and only those were rewritten.

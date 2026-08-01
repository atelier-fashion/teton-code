---
id: TASK-029
title: "Mechanical import-after-build ordering assertion (BR-6)"
status: draft
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

## Acceptance Criteria

- [ ] Case fails when the import step is moved above the build step in a scratch copy (AC-4 mutation, performed and recorded in the case comment) and when either anchor string is absent
- [ ] Runs in ci.yml's tooling job via the existing selftest invocation — no CI changes needed (verified by running the suite)
- [ ] shellcheck clean; suite fully green; report the new total

## Technical Notes

Mirror the cross-file consistency case's file-reading pattern
(selftest.sh ~1876-1895). Anchor on the step NAME for import (TASK-028
keeps it verbatim) and on the `package.sh` phase-argument invocations for
build/pack — these are the load-bearing strings; the case comment must say
so, so a future rename updates the assertion in the same commit.

---
id: BUG-217
title: "The release self-test reports a matching output assertion as FAIL when grep exits before printf finishes writing"
status: resolved
severity: medium
created: 2026-09-06
updated: 2026-09-06
resolved: 2026-09-06
component: "tools/release"
domain: "ci"
stack: ["bash", "ci"]
concerns: ["developer-experience", "reliability"]
tags: ["selftest", "pipefail", "sigpipe", "flake", "required-check"]
introduced_by: ["REQ-550"]
attribution: manual
---

## Description

`tools/release/selftest.sh` runs under `set -euo pipefail`. Its `expect_output`
helper tested the captured case output with `printf '%s' "$CASE_OUT" | grep -qF
-- "$2"`. `grep -q` exits on its first match; when the output is longer than
what `printf` has written by then, `printf` takes SIGPIPE, the pipeline's
status is non-zero under `pipefail`, and the assertion reports the **match**
as `FAIL [output does not contain: …]`. The `release tooling` job is a
required check, so the flake blocks any PR at random.

Seen on PR #305 (a `.adlc/`-only wrapup commit): the batch-drift case printed
`printf: write error: Broken pipe` and then `FAIL ... and shows both digests
[output does not contain: recorded:]`, while the captured output the report
echoed back plainly contained the digest lines. A rerun of the same job on the
same commit passed.

## Reproduction Steps

1. Any commit; open a PR.
2. Watch the `release tooling (actionlint · shellcheck · selftest)` job over
   several runs. The `verify-attestations-batch` group's long-output cases fail
   intermittently with the broken-pipe line immediately above the FAIL.

## Expected Behavior

An assertion that reads "the output contains this fixed string" passes exactly
when the string is present, regardless of how quickly the reader exits.

## Actual Behavior

It passes when `printf` finishes before `grep` finds the string and fails
otherwise. The result depends on scheduler timing, not on the output.

## Environment

- GitHub-hosted `ubuntu-latest`; bash 5.x. Any host with `pipefail`.

## Root Cause

A `pipefail` pipeline whose producer can be killed by its consumer's early
exit. The file already records why `grep -qF` is the wrong instrument for this
check in two other places (the empty-needle regression in the smoke group, and
`verdict_names`, which uses `case` citing LESSON-442); `expect_output` was the
one assertion helper that had not been moved onto the same footing.

## Resolution

`expect_output` matches with `case "$CASE_OUT" in *"$2"*)`: no subprocess, no
pipe, no signal, and the same fixed-string semantics. An empty needle is
refused with its own FAIL line instead of matching everything, closing the
`*""*` hole the switch would otherwise open (the same hole the smoke group's
`grep -qF -- ""` regression documents).

## Files Changed

- `tools/release/selftest.sh` — `expect_output` uses `case`; empty needle refused

---
id: TASK-022
title: "package.sh signing phase and smoke.sh signature assertion"
status: draft
parent: REQ-550
created: 2026-07-31
updated: 2026-07-31
dependencies: [TASK-021]
---

## Description

Wire signing into the artifact path (ADR-550-1): `package.sh` signs both
binaries between build and tar when `TETON_SIGN_IDENTITY` is set, hard-fails
on any signing error, and `smoke.sh` asserts the signature on macOS targets
(via TASK-021's gate) so a tarball with unsigned/ad-hoc binaries can never
pass the per-target smoke.

## Files to Create/Modify

- `tools/release/package.sh` — after the build/staging step and before `tar`: when `TETON_SIGN_IDENTITY` is non-empty, `codesign --sign "$TETON_SIGN_IDENTITY" --timestamp --options runtime` each of `teton`/`teton-code`, then `codesign --verify --strict` each; any failure exits 70 (EX_SOFTWARE, matching the existing missing-binary code). When unset, note "unsigned (dev build)" on stdout
- `tools/release/smoke.sh` — after the binary-existence check and before the version assertions: on darwin targets, call `verify-signature.sh` against the extracted binaries with the expected team id (new positional/env arg `TETON_SMOKE_TEAM_ID`); map 65→`fail`, 75→exit 75 (UNCHECKED), preserving the existing pass/fail counter style. On the Linux target, emit one explicit "artifact is unsigned in v1 (by design)" line — never silently skip

## Acceptance Criteria

- [ ] `package.sh` with `TETON_SIGN_IDENTITY` set and no usable identity fails loudly (non-zero, message names signing) — it must be impossible to produce an unsigned tarball from a signing-requested invocation (BR-2)
- [ ] `package.sh` without the var behaves byte-identically to today apart from the one informational line
- [ ] smoke.sh signature assertion uses TASK-021's script (no second codesign invocation path to drift), and its absence of a team-id arg on darwin is a usage error, not a skip
- [ ] shellcheck clean; selftest still 98+ green (existing cases unbroken)

## Technical Notes

Signing must happen on the staged copies that go into the tarball, not the
target/ originals, so the shipped bytes are the verified bytes. `--options
runtime` (hardened runtime) is required for future notarization (spec OQ-3)
and harmless now. LESSON-455: the property is "every shipped macOS binary is
signed" — both binaries, both macOS targets, no per-file drift.

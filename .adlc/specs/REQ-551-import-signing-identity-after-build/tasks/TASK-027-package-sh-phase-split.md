---
id: TASK-027
title: "package.sh phase argument: build / pack / all"
status: draft
parent: REQ-551
created: 2026-08-01
updated: 2026-08-01
dependencies: []
---

## Description

Implement ADR-551-1's phase split in package.sh with byte-compatible
default, and adapt/extend the selftest coverage so every phase and the
cross-boundary BR-2 property are proven.

## Files to Create/Modify

- `tools/release/package.sh` — optional 4th arg `phase` (all|build|pack, default all; anything else → 64): `build` = validation → cargo build → stage binaries+LICENSE+README into `<outdir>/stage-<target>/`, no signing-tool resolution, TETON_SIGN_IDENTITY ignored; `pack` = seam guard → tool resolution → sign→verify (keyed on TETON_SIGN_IDENTITY, unchanged semantics/messages) → tar → sha256, consuming the staging dir; missing dir or either binary → 70 with a message naming the phase contract (BR-2 cross-boundary); `all` = both in-process (current behavior, current output bytes)
- `tools/release/selftest.sh` — adapt the stubbed-cargo group: drive `build` alone (stages, no codesign invoked — assert the stand-in was NOT called), `pack` alone against a pre-staged dir (all existing sign-accept/sign-reject/verify-reject/no-identity cases), `all` for compatibility (one case asserting identical flow to today), `pack` without a staging dir → 70, `pack` with one binary missing → 70, `build` with TETON_SIGN_IDENTITY set → identity ignored and NO tool resolution (BR-1: the build phase must not touch signing even when the var leaks in)

## Acceptance Criteria

- [ ] Default `all` invocation byte-compatible: existing runbook/docs commands work unchanged (AC-3 floor: full suite green, ≥261 cases)
- [ ] `pack` can never emit a tarball without a complete staging dir (70), and `build` never resolves or invokes the signing tool (BR-1/BR-2)
- [ ] Existing exit taxonomy preserved exactly (64/70/75/cargo passthrough); shellcheck clean
- [ ] Mutation check: break the pack-phase staging-dir guard, watch its case fail, restore (LESSON-454)

## Technical Notes

The staging dir must be deterministic (workflow passes it implicitly via
outdir+target) and documented in the header. Reuse the existing staging
code — this is a reorder into functions, not a rewrite (spec assumption).
Keep the seam guard in `pack` and `all` only; `build` runs no seam tool.

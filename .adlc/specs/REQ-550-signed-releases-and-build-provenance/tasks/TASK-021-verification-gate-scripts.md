---
id: TASK-021
title: "Signature and attestation gate scripts with seam-testable exit taxonomy"
status: draft
parent: REQ-550
created: 2026-07-31
updated: 2026-07-31
dependencies: []
---

## Description

Create the two verification gate scripts and their shared helpers
(ADR-550-4): `verify-signature.sh` wrapping `codesign`, and
`verify-attestation.sh` wrapping `gh attestation verify`, both classifying
exits per the house taxonomy (0 PASS / 65 FAILED / 75 UNCHECKED / 64 usage)
and honouring tool-override seams (`TETON_CODESIGN`, `TETON_GH`) so the
Linux selftest can drive them.

## Files to Create/Modify

- `tools/release/verify-signature.sh` — new gate: args `<binary|tarball> <team-id>`; extracts tarballs to a temp dir; runs `codesign --verify --strict` + `codesign -dv` on `teton` and `teton-code`, asserting "Developer ID Application" and the team id substring; 65 on any rejection, 75 when the codesign tool is unavailable, 64 on bad args
- `tools/release/verify-attestation.sh` — new gate: args `<artifact> <repo>`; runs `gh attestation verify <artifact> --repo <repo>`; 65 when verification fails, 75 when `gh` is missing or errors for a non-verification reason (network/auth), 64 on bad args
- `tools/release/lib.sh` — add `tool_or_unchecked` helper (resolve override env var → PATH lookup → return 75-semantics failure) following the `sha256_of` availability-check pattern

## Acceptance Criteria

- [ ] Both scripts pass shellcheck and follow smoke.sh's `set -uo pipefail` + explicit exit-constant style
- [ ] An unforeseen internal failure exits 75, never 65 (LESSON-442: 65 must be unforgeable as "bytes are bad")
- [ ] With `TETON_CODESIGN`/`TETON_GH` pointing at stand-ins: rejecting stand-in → 65, absent tool → 75, accepting stand-in → 0 (proven in TASK-025's selftest cases)
- [ ] Real-tool paths documented in headers: signature gate runs on macOS legs only; attestation gate anywhere `gh` exists

## Technical Notes

Follow `verify-version.sh` as the structural model for a standalone gate
script. The seam env vars default to the real tools; the override exists for
selftest, mirroring smoke.sh's `TETON_SMOKE_*_DEADLINE_SECS` precedent.
Never print secret material; identity strings and team ids are public.

---
id: TASK-025
title: "Selftest known-bad fixtures: both new gates must go red"
status: complete
parent: REQ-550
created: 2026-07-31
updated: 2026-07-31
dependencies: [TASK-022]
---

## Description

Prove the gates per LESSON-454: extend selftest.sh with fixtures and cases
that drive `verify-signature.sh` and `verify-attestation.sh` (and their
smoke.sh integration) to every classification — red on known-bad input,
UNCHECKED on missing tooling, green on good input — using the TASK-021
tool-override seams, since the CI tooling job runs on Linux.

## Files to Create/Modify

- `tools/release/selftest.sh` — new case group after the existing smoke block: (a) stand-in `codesign` builders (accepting / rejecting / absent) driving verify-signature.sh to 0 / 65 / 75, including the ad-hoc-signed stand-in tarball case through smoke.sh's darwin path (smoke invoked with a forced darwin target and the seam); (b) stand-in `gh` builders driving verify-attestation.sh to 0 / 65 / 75, including a byte-flipped tarball case where the rejecting stand-in models real `gh attestation verify` behavior on tampered bytes; (c) a case asserting smoke.sh's Linux path emits the explicit "unsigned in v1" line rather than silently skipping; (d) a case asserting an unforeseen gate-script crash classifies as 75, not 65 (LESSON-442)

## Acceptance Criteria

- [ ] Every new case uses `expect_exit`/`expect_output` and the `make_standins`/`make_tarball` builders' style; suite total rises accordingly and passes on Linux CI
- [ ] The rejecting-stand-in cases are genuine known-bad fixtures (BR-5 / AC-3): comment in each names the violation it models (tampered artifact, ad-hoc signature) per LESSON-454's "build the known-bad input and watch it go red"
- [ ] Assertion provenance audit: no pass condition is satisfiable by harness intervention (no watchdog-supplied exits) — reviewed explicitly in the case comments
- [ ] shellcheck clean; existing 98 cases untouched and green

## Technical Notes

The seam proves the classification logic, not Apple's or GitHub's tooling —
the real-tool legs run in the release pipeline itself (ADR-550-4), and that
split is stated in the case-group header comment (LESSON-433: unrun legs
recorded as unrun, never extrapolated).

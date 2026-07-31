---
id: TASK-023
title: "release.yml: keychain import, environments, attestation, verify gates"
status: draft
parent: REQ-550
created: 2026-07-31
updated: 2026-07-31
dependencies: [TASK-022]
---

## Description

The workflow half (ADR-550-2/3): ephemeral-keychain signing setup on macOS
build legs, `environment:` declarations on credential-bearing jobs,
provenance attestation of the tarballs, and attestation verification as a
release gate plus an end-to-end check in verify-install.

## Files to Create/Modify

- `.github/workflows/release.yml` — (a) `build` job: declare `environment: release-signing`; on darwin legs add an "import signing identity" step (decode `secrets.MACOS_CERT_P12` → temp file; `security create-keychain` with run-random password; `security import` with `secrets.MACOS_CERT_PASSWORD`; `set-key-partition-list`; add to search list), export `TETON_SIGN_IDENTITY="Developer ID Application: Atelier Fashion LLC (${{ vars.APPLE_TEAM_ID }})"` and `TETON_SMOKE_TEAM_ID` for the package/smoke steps, and an `if: always()` cleanup step deleting keychain + temp p12; (b) `release` job: add `id-token: write` + `attestations: write` permissions, `actions/attest-build-provenance` (SHA-pinned, house audit posture) with the three tarballs as subjects, and post-publish `verify-attestation.sh` per asset (65/75 mapped to the existing FAILED/UNCHECKED messages); (c) `bump-formula` job: `environment: tap-publish`; (d) `verify-install` job: run `verify-attestation.sh` against the brew-downloaded tarball

## Acceptance Criteria

- [ ] actionlint clean; every new third-party action SHA-pinned (REQ-548 audit posture; LESSON-455 property: ALL of them)
- [ ] The keychain cleanup step runs on failure paths (`if: always()`)
- [ ] A darwin leg cannot reach the smoke step unsigned: missing/invalid cert fails the import or package step, never falls through (BR-2)
- [ ] Attestation gate failure blocks `bump-formula` via the existing needs-graph (no new ordering edge required — assert this in a comment)
- [ ] `secrets.HOMEBREW_TAP_TOKEN` references unchanged in text but now resolve via the tap-publish environment (comment records this)

## Technical Notes

Environments were created and rule-configured on 2026-07-31 (spec Verified
Inventory) — this task only *declares* them. The Linux build leg shares the
`environment: release-signing` declaration harmlessly. Release notes gain a
one-line honesty statement: macOS binaries signed (team 545BU9G9D6), Linux
unsigned in v1 (BR-6). LESSON-441 applies to the fix pass: any adjustment
here re-runs actionlint + selftest, not just eyeballs.

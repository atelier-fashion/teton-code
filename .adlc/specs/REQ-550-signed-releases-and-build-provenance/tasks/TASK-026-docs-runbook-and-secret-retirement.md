---
id: TASK-026
title: "Docs, runbook verification commands, and repo-secret retirement plan"
status: complete
parent: REQ-550
created: 2026-07-31
updated: 2026-07-31
dependencies: [TASK-023]
---

## Description

The paper trail (BR-3's runbook half, BR-6's honesty, AC-4's retirement
sequence): user-facing verification commands, runbook updates, and the
explicit ordered checklist for deleting the repository-level
`HOMEBREW_TAP_TOKEN` and running the AC-4 negative probe.

## Files to Create/Modify

- `README.md` — a short "Verify a release" section: `gh attestation verify teton-v<X.Y.Z>-<target>.tar.gz --repo atelier-fashion/teton-code` plus the macOS `codesign --verify --strict` one-liner; states Linux artifacts are unsigned in v1
- `docs/release-runbook.md` — attestation-verify step beside the existing checksum verification; Developer ID identity-stability contract (BR-1) and what changes at cert renewal (anchor is team id + identifier, not the leaf cert); troubleshooting for cert-expired/absent (the release fails loudly by design, BR-2)
- `docs/homebrew-tap-setup.md` — token is environment-scoped under `tap-publish`; note the retirement sequence below
- `packaging/homebrew/teton.rb.tmpl` — one comment line noting shipped macOS binaries are Developer ID signed (team 545BU9G9D6)
- `docs/release-runbook.md` (same file, distinct section) — **secret retirement checklist**: (1) this REQ merged; (2) one release completes green end-to-end (proves environment resolution); (3) delete repo-level `HOMEBREW_TAP_TOKEN`; (4) AC-4 probe: manually dispatch a minimal workflow on a non-release branch that requests `environment: tap-publish` and record its refusal; (5) record environment settings state in the REQ

## Acceptance Criteria

- [ ] Runbook commands are copy-pasteable and match the exact asset naming produced by package.sh
- [ ] Retirement checklist is ordered, each step a checkbox, with the "why not earlier" rationale inline (deleting before a green release breaks bump-formula)
- [ ] No doc claims signing for Linux or notarization for anything (BR-6, OQ-3 deferred)

## Technical Notes

AC-6 (Keychain-grant survival across two signed releases) is staged: add it
to the runbook's post-release checks as "first exercisable at the second
signed release", mirroring how REQ-548 staged its upgrade AC.

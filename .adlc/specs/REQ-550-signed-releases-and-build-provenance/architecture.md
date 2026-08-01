# REQ-550 Architecture — Signed releases and build provenance

Grounding: the release pipeline's job graph is `preflight → build (3-target
matrix) → release → bump-formula → verify-install → dispatch-site-deploy`
(release.yml), with credentials touched only in `bump-formula`
(`HOMEBREW_TAP_TOKEN`) and `deploy-site.yml`'s `deploy` job (WIF,
`id-token: write` already present). Gates share one exit taxonomy: 0 PASS,
64 usage, 65 FAILED (bytes are bad), 75 UNCHECKED (could not verify) —
enforced in smoke.sh/selftest.sh and mapped in release.yml. Selftest builds
known-bad stand-ins via `make_standins`/`make_tarball` and drives gates red
deliberately (LESSON-454 lineage).

GitHub-side state (configured 2026-07-31, rules amended 2026-08-01, recorded
in the spec's Verified Inventory): environments `release-signing` (secrets
`MACOS_CERT_P12`, `MACOS_CERT_PASSWORD`; variable `APPLE_TEAM_ID`),
`tap-publish` (secret `HOMEBREW_TAP_TOKEN` copy) and `site-deploy` — all three
now carrying the same deployment rules, **tag `v*.*.*` + branch `main`**.
`site-deploy` always did; `main` was added to the other two during the Phase-5
verify pass so the mandatory dry run can run at all (see ADR-550-2's
consequence below). The repo-level `HOMEBREW_TAP_TOKEN` remains until the
workflow declares environments (deleting first would break the next release).

## ADR-550-1: Sign in `package.sh` between build and tar, keyed on an explicit signing request — never on certificate presence

**Decision**: `package.sh` grows a signing phase between `cargo build` and
tarball assembly, activated by `TETON_SIGN_IDENTITY` (the full identity
string, e.g. `Developer ID Application: Atelier Fashion LLC (545BU9G9D6)`).
When set: `codesign --sign "$TETON_SIGN_IDENTITY" --timestamp --options
runtime` both `teton` and `teton-code`, then `codesign --verify --strict`
each — any failure exits 70 (EX_SOFTWARE) and the leg dies. When unset
(local dev builds), binaries stay ad-hoc — dev builds are not releases. The
release workflow sets `TETON_SIGN_IDENTITY` unconditionally on both macOS
legs, derived from `vars.APPLE_TEAM_ID` — so the "should we sign?" predicate
is *structural* (macOS release leg ⇒ sign or die), never "is the cert
available?" (BR-2; LESSON-443's self-disabling-guard shape).

**Rationale**: signing must precede tar (the tarball ships the signed
Mach-Os); `package.sh` already owns the build→tar seam and the exit
taxonomy. Putting the *decision* in the workflow and the *mechanics* in the
script keeps the script testable by selftest via a codesign seam.

## ADR-550-2: Throwaway keychain per run, imported and destroyed in the workflow

**Decision**: the macOS build legs get an "import signing identity" step
before `package.sh`: decode `MACOS_CERT_P12` to a temp file, `security
create-keychain` with a run-random password, `security import` with the
cert password, `security set-key-partition-list` (unlocks codesign use
without UI), add to the search list; an `if: always()` cleanup step deletes
the keychain and the temp .p12. The `build` job declares
`environment: release-signing` (all three legs — the Linux leg simply never
reads the secrets; conditional per-matrix environments are not expressible,
and the tag rule already gates when the job can run at all).

**Rationale**: the P12-in-secret + ephemeral-keychain pattern is standard on
hosted runners, leaves no persistent key material, and was the spec's OQ-2
recommendation. One environment on the whole matrix job beats splitting
signing into a separate job, which would break package.sh's
build→sign→tar atomicity.

**Consequence found in verify (2026-08-01)**: putting an environment on the
whole `build` job also gates *when the job may run at all*, so the initial
tags-only rule on `release-signing` (and `tap-publish`, for `bump-formula`)
silently made the `workflow_dispatch` dry run impossible — the jobs were
blocked before their first step, and the only way to exercise the pipeline was
to spend a tag. Resolved by adding branch `main` to both rules, which BR-4
already permits ("`v*.*.*` tags and/or `main`"); every other ref is still
refused, so the AC-4 negative probe is unaffected.

## ADR-550-3: Attest and verify in the `release` job; the verify gate blocks the tap bump

**Decision**: the `release` job (which already downloads all tarballs,
recomputes checksums, and publishes) gains job permissions `id-token: write`
+ `attestations: write`, an `actions/attest-build-provenance` step whose
subjects are the three tarballs (pinned by SHA per the REQ-548 audit
posture), and — after `gh release create` — a verification gate that runs
`tools/release/verify-attestation.sh` per published asset. A non-zero gate
fails the `release` job, and since `bump-formula` `needs: release`, a
failed verification means the tap never advances (same invariant as BR-4 in
REQ-548: no published release with a formula pointing at unverified bytes).
`verify-install` additionally runs the same verification against the
brew-downloaded tarball, making the runbook command
(`gh attestation verify <file> --repo atelier-fashion/teton-code`)
end-to-end-proven every release.

**Rationale**: attestation must happen where the artifacts and the OIDC
token are; verification against the *published* assets (not the local
copies) is what a user's runbook invocation actually exercises.

## ADR-550-4: Gates are seam-testable wrapper scripts; selftest proves red via injected stand-in tools

**Decision**: two new gate scripts with lib.sh-style helpers —
`verify-signature.sh <binary-or-tarball>` (wraps `codesign --verify
--strict` + `codesign -dv` asserting "Developer ID Application" and the
team id) and `verify-attestation.sh <artifact>` (wraps `gh attestation
verify`) — both classifying exits per the house taxonomy: 0 PASS, 65 FAILED,
75 UNCHECKED (tool missing / could not verify), with LESSON-442's rule that
an unforeseen failure lands on 75, never 65. Each honours a tool-override
seam (`TETON_CODESIGN`, `TETON_GH`) so selftest — which runs on the Linux
`tooling` CI job where `codesign` does not exist — can inject stand-ins:
a stand-in that rejects (tampered/ad-hoc case) must drive the gate to 65,
a missing tool to 75, an accepting stand-in to 0, per LESSON-454 ("a gate
is only a gate if a known-bad input makes it go red"). The *real*
tools run in the release pipeline itself: signature verification in the
macOS smoke legs (where codesign exists natively; the Linux leg records
"unsigned in v1, by design"), attestation verification in the release and
verify-install jobs.

**Rationale**: seams keep the classification logic provable in CI on every
PR; the per-platform split follows LESSON-433 (never extrapolate a
verification claim across platforms — each leg proves its own artifact).

## Corrections to exploration output (recorded for the implementer)

- Signing secrets live in **release-signing**, not tap-publish. The spec's
  Verified Inventory is authoritative over the mapper's table. (This bullet
  originally also said tap-publish was tags-only — superseded 2026-08-01:
  `main` was added to both `release-signing` and `tap-publish`, per the
  Grounding note above and ADR-550-2's consequence.)
- `GCP_*` values are read via WIF config in deploy-site.yml but there are
  **no repo-level GCP secrets** to move; BR-4's remaining work there is the
  `environment: site-deploy` declaration + the WIF `assertion.ref`
  condition (GCP-side, human step, documented not scripted).
- `APPLE_TEAM_ID` is an **environment variable on release-signing**, not a
  repository variable.

## Proposed change to `.adlc/context/architecture.md`

After completion, append ADR-008 (signed releases + provenance) summarizing
ADR-550-1/3 and the environment-gating posture; also note conventions.md's
binary-name line was corrected in this phase.

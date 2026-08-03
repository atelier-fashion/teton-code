---
id: REQ-551
title: "Import the signing identity only after untrusted compilation"
status: complete
deployable: true
created: 2026-08-01
updated: 2026-08-03
component: "distribution/release"
domain: "distribution"
stack: ["github-actions", "ci", "keychain", "rust"]
concerns: ["security", "reliability"]
tags: ["keychain-window", "import-after-build", "build-rs", "supply-chain", "codesign", "release-pipeline"]
---

## Description

REQ-550's release pipeline imports the Developer ID signing identity into an
unlocked throwaway keychain *before* `cargo build --release --features
tetond/llama` runs. `set-key-partition-list -S apple-tool:,apple:` grants
prompt-free codesign access and auto-lock is disabled — necessarily, since
signing happens un-attended later in the job. The consequence, found by
REQ-550's own verify pass and recorded there as user-accepted risk
(2026-08-01): for the ~30 minutes of untrusted compilation — every
third-party crate's `build.rs` plus the from-source llama.cpp cmake build,
all running as the same uid — a hostile build script can invoke `codesign`
against the release identity and sign arbitrary bytes. The p12 file itself
was already narrowed (removed seconds after import, and password-encrypted
regardless); the unlocked keychain is the sharper capability and is untouched.

This REQ closes that window: no signing credential material — imported
identity, unlocked keychain, or decoded p12 — may exist on the runner while
any untrusted code executes. The identity arrives only after compilation
completes, immediately before the sign→verify→tar sequence
(import → sign → early destroy; the tarball smoke and the artifact upload
run afterwards with no keychain present), and every guarantee REQ-550
established must survive the restructuring.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| BuildPhase | outputs | staged binaries | produced by untrusted compilation; no credential material present during execution (BR-1) |
| SignPhase | inputs | staged binaries + imported identity | begins only after BuildPhase exits; runs sign→verify→tar unchanged (BR-3) |
| KeychainWindow | open_from | timestamp | first `security import` — MUST be after the last untrusted process exits (BR-1) |
| KeychainWindow | open_until | timestamp | cleanup step; window covers only first-party signing/packaging code |
| PhaseContract | package.sh | interface | build and sign+tar phases separately invocable or reordered; selftest coverage survives (BR-4) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| build_completed | cargo/cmake exit, per matrix leg | staged unsigned binaries |
| identity_imported | after build_completed only | keychain path (ordering is the guarantee) |
| ordering_violated | import observed before build in workflow structure | selftest/lint assertion failure (BR-6) |

## Business Rules

- [x] BR-1: No signing credential material (imported identity, unlocked
      keychain, decoded p12) exists on the runner while any third-party
      code (`build.rs`, cmake, or any dependency-supplied process) executes.
      Stated as a property over the whole job, not a step ordering in one
      file (informed by LESSON-455, REQ-550).
- [x] BR-2: REQ-550 BR-2 survives unchanged: signing stays keyed on the
      explicit request (`TETON_SIGN_IDENTITY`), never on cert/tool presence,
      and a signing-requested run can never produce an unsigned tarball —
      including across the new phase boundary (a build phase that succeeds
      followed by a sign phase that never runs must fail the leg, not ship;
      informed by LESSON-443, LESSON-447).
- [x] BR-3: The sign→verify→tar ordering is preserved: the bytes entering
      the tarball are the bytes `codesign --verify --strict` accepted, with
      no untrusted execution between verify and tar (informed by
      LESSON-445's stage-then-commit shape).
- [x] BR-4: `package.sh`'s contract change keeps its selftest coverage:
      every existing signing case (sign-accept, sign-reject, verify-reject,
      tool-missing → 70, unsigned-dev note) keeps passing or has an
      equivalent against the new phase interface, and the known-bad fixtures
      still drive the gates red (informed by LESSON-454, REQ-550 BR-5).
- [x] BR-5: The `workflow_dispatch` dry-run path (from `main`, no tag)
      continues to build, sign, smoke, attest, and verify end-to-end.
- [x] BR-6: The new ordering is mechanically asserted, not commented: an
      automated check (selftest case or CI lint) fails if the import step
      precedes the build step in `release.yml`'s job structure — a comment
      claiming the ordering is not a guard (informed by LESSON-443,
      LESSON-454, and REQ-550's verify pass, where a comment claimed a
      guard that did not exist).

## Acceptance Criteria

- [x] AC-1: In `release.yml`, the identity-import step executes after the
      step that runs `cargo build` (and any other dependency-executing
      step) on both macOS legs; the keychain-open window covers only
      first-party signing/packaging code. Verified by reading the job's
      step order AND by BR-6's mechanical assertion.
- [ ] AC-2: A full release (or main-dispatched dry run) goes green
      end-to-end with the restructured job: build → import → sign → verify
      → tar → smoke → attest → verify-attestations.
- [x] AC-3: `tools/release/selftest.sh` remains fully green with coverage
      equivalent to REQ-550's final state (257 cases at time of writing),
      including the package.sh phase-contract cases under the new
      interface.
- [x] AC-4: A deliberate ordering regression (moving the import step above
      the build step) makes BR-6's assertion fail — proven once with the
      known-bad mutation and recorded (informed by LESSON-454).
- [x] AC-5: shellcheck + actionlint clean; no new secret-interpolation
      into `run:` bodies; the `if: always()` keychain cleanup still covers
      all failure paths of the narrowed window.

## Deviations (verify pass, 2026-08-03)

Recorded rather than silently absorbed, so a reader comparing the spec to the
merged pipeline finds the difference here instead of deriving it.

- **AC-2 is staged, not skipped.** "A full release (or main-dispatched dry run)
  goes green end-to-end with the restructured job" cannot be exercised from
  this REQ's branch: the `release-signing` and `tap-publish` environments admit
  exactly two ref shapes — the `main` branch and `v*.*.*` tags — so a dispatch
  from `feat/REQ-551-…` is refused by the deployment rules before the job
  starts, and there is no ref this branch could dispatch from that would reach
  the certificate. First exercised by the post-merge dry run from `main`, or by
  the next real release, whichever comes first. This is the same posture and
  the same precedent REQ-550 set for its own environment-gated criteria (see
  runbook §11 steps 2/4/6): a criterion nobody *could* have run is recorded as
  unrun, never assumed and never quietly dropped.
- **ADR-551-1's "moved verbatim" means the executable body, not the whole
  step.** The import step's shell body is byte-identical to REQ-550's — checked
  by the reflector, not asserted — and its `- name:` line is unchanged, which
  is what the ordering assertion keys on. What did change is 16 annotation
  strings inside it, retargeted under TASK-029's recorded scope extension:
  before the reorder they told the reader "Nothing was built," which stopped
  being true the moment the compile moved above the import. They now say the
  build already finished and the leg fails before anything is signed. Correcting
  a message that had become false is not a deviation from "verbatim"; leaving it
  in place to preserve the word would have been.
- **The cross-boundary staging guard is a superset of what ADR-551-1
  specified.** The ADR describes `pack` refusing a missing or incomplete
  staging directory in terms of the two binaries. As implemented it also checks
  the ride-alongs — `LICENSE` and `README.md` — because every member of the
  tarball comes out of the staging directory and `tar` failing on a missing one
  would report a file, from outside `package.sh`'s exit taxonomy, rather than
  reporting a phase. Wider than specified, in the direction the rule points.
- **`package.sh` grew changes the ADRs never described**, listed here so that
  reconciling the merged script against ADR-551-1 does not require diffing them
  line by line. All of them arrived in the verify passes (2026-08-03):
  - **`.stage-meta`**, the handoff manifest (version + a sha256 per member),
    written atomically via a temp name and removed with the whole half-made
    stage when a digest cannot be computed. Its framing in the code and the
    runbook is deliberately narrow: it detects a **stale, partial,
    version-skewed or accidentally altered** stage, and it is
    **unauthenticated** — anything that can write `<outdir>` rewrites the
    manifest too. That axis stays with the provenance attestation
    (ADR-550-3); the manifest is not a second one.
  - **The scratch-sign design.** `pack` copies the two binaries to a
    trap-cleaned scratch directory, signs and verifies *those*, and tars them
    beside the stage's `LICENSE`/`README.md` — the member list and its order
    are unchanged. `codesign` rewrites the file it signs, so signing in place
    made "a failed pack keeps the stage for a cheap retry" false in exactly
    the case it was written for (`--sign` succeeds, `--verify` rejects): the
    retry hit the manifest check and was refused as a tampered stage. The
    stage is now written by `pack` on one path only — the success-path
    consume.
  - **Argument refusals**: a fifth argument, and a *three*-argument invocation
    whose `[outdir]` is a phase name (`package.sh <target> <version> pack` is
    an `all` into a directory called `pack` — the pre-REQ-551 ordering reached
    by a typo). Gated on `$# -eq 3`, so a spelled-out fourth argument makes it
    an ordinary pack into `./pack`.
  - **Per-member file-type checks** in the pack guard: symlink, directory, and
    anything else that is not a regular file, each `70` naming itself. Without
    them a directory member surfaces as `75` "no sha256 tool on this machine",
    which is the wrong code and the wrong diagnosis.
  - **`lib.sh`'s `tool_or_unchecked`** rejects a non-absolute resolution when
    no override was given: an exported shell function planted through
    `$BASH_ENV` resolves as a bare name, and would otherwise *be* the signer.
- **The three identity-touching workflow steps carry an environment preamble**
  that no ADR describes (import, sign-and-package, early destroy): `BASH_ENV`/
  `ENV`/`CDPATH` unset, the tools' shell functions unset, `PATH` pinned to the
  system directories, and `security` invoked absolutely. The reorder put those
  steps *downstream* of untrusted compilation while `$GITHUB_ENV`/
  `$GITHUB_PATH` are applied between steps. Recorded with its limit: this
  closes the in-band channels only — a persistent background process from the
  build is runner-level compromise, which OQ-1 (split jobs) is the recorded
  answer to and this REQ does not claim to address.

## External Dependencies

- None beyond REQ-550's (same secrets, same environments, same actions).

## Assumptions

- The same-job restructuring (import step moved between build and a
  sign+package invocation) is achievable without artifact hand-off between
  jobs; if architecture instead chooses split jobs, the staged binaries
  cross a job boundary as workflow artifacts and the attestation story is
  unchanged (they are attested post-sign in the release job, as today).
- `package.sh`'s phase split is an interface change only — the sign→verify→
  tar implementation from REQ-550 is reused, not rewritten.
- REQ id allocated with remote verification (no degradation warning).

## Open Questions

- [ ] OQ-1: Same-job step reorder (cheapest; keychain window narrows to
      minutes) vs split build/sign jobs (stronger isolation — the signing
      job never runs untrusted code at all — but adds artifact upload/
      download of staged binaries and a second macOS runner spin-up per
      leg)? Recommend same-job reorder in v1; record split-jobs as the
      future hardening if runner-level compromise enters the threat model.
- [ ] OQ-2: Should BR-6's mechanical assertion live in selftest (grep-based
      structural check on release.yml, runs on every PR) or as a dedicated
      actionlint-style CI step? Recommend selftest — it already carries the
      team-id consistency case precedent.

## Out of Scope

- Changing what is signed, attested, or verified (REQ-550's gates are
  frozen surface here).
- Self-hosted runners, HSM/cloud-signing services (e.g. notarytool or
  cloud KMS signing) — different trust model, separate REQ if ever needed.
- The Linux leg (no signing, no keychain — untouched).
- Notarization (REQ-550 OQ-3, still deferred).

## Retrieved Context

- REQ-550 (spec, score 14): Stable code-signing identity and build provenance for released binaries
- LESSON-454 (lesson, score 9): A gate whose kill supplies the failure signal
- LESSON-455 (lesson, score 6): Scope a fix to the property, not to the file the finding cited
- LESSON-443 (lesson, score 5): A guard keyed on a feature's absence disables itself
- LESSON-444 (lesson, score 5): A C library's assert is a process abort
- LESSON-445 (lesson, score 5): Stage, then commit only after re-checking authority
- LESSON-433 (lesson, score 4): Single-platform verification gives false confidence
- LESSON-457 (lesson, score 3): An executable's filename is a trust surface
- LESSON-441, LESSON-442, LESSON-446, LESSON-447, LESSON-448, LESSON-449, LESSON-450 (lesson, score 3 each)

Note: all retrieved bodies were already in conversation context from this
session's REQ-550 pipeline (the delegate gate reported no-binary; no re-reads
were performed). REQ-544/547/548/549 are excluded by the status filter
(`complete`).

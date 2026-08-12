---
id: REQ-548
title: "One-command Homebrew install and the tetoncode.ai landing page"
status: complete
deployable: true
created: 2026-07-24
updated: 2026-08-12
component: "distribution/release"
domain: "distribution"
stack: ["rust", "github-actions", "homebrew", "ci", "static-site"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["homebrew", "tap", "formula", "release-pipeline", "prebuilt-binaries", "brew-services", "landing-page", "code-signing"]
---

## Description

Today, installing Teton Code means cloning the repo and running
`cargo build --workspace --release --features tetond/llama` — which requires a
Rust toolchain, cmake (llama.cpp builds from source), and knowledge of a
feature-flag spelling that, if gotten wrong, produces a daemon that installs
weights it can never load. The README still says "pre-alpha — product spec
stage." There is no distribution story and no web presence at the brand domain.

This REQ makes the top of the funnel one command:

```
brew install atelier-fashion/tap/teton
```

and one page: `https://tetoncode.ai`, a static landing page with the product
overview and that install command.

Three properties of the existing system make this cheap and safe, and the spec
leans on all three:

1. **The first-run consent flow (REQ-547)** keeps the installed artifact small:
   binaries only. Model weights arrive on first `teton` run, hardware-matched,
   with explicit consent — so the brew package never ships gigabytes.
2. **The release-build seam refusal (REQ-547 DECISION 3)** means shipping
   release binaries is safe against the test-seam surface by design: a release
   `tetond` refuses `TETON_TEST_SEAMS=1` loudly rather than honouring catalog
   swaps or fabricated hardware.
3. **The daemon/CLI split** maps directly onto Homebrew's service model:
   `brew services start teton` runs `tetond` under launchd, which is a better
   daemon story than `tetond &` in a terminal.

Distribution route: our own tap (`atelier-fashion/homebrew-tap`) serving
prebuilt per-platform binaries from GitHub Releases. homebrew-core is
explicitly out of scope until the project meets notability thresholds;
build-from-source formulas are rejected because they would reimpose the
Rust+cmake burden this REQ exists to remove.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| Release | tag | string | required; `vX.Y.Z`; MUST equal the workspace `Cargo.toml` version (BR-3) |
| Release | artifacts | list | one tarball per supported target, plus a checksums file |
| ReleaseArtifact | target | enum(aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu) | required |
| ReleaseArtifact | contents | list | exactly `teton` and `tetond`, release profile, built with `tetond/llama` |
| ReleaseArtifact | sha256 | string | required; computed from the uploaded bytes, never hand-typed (BR-5) |
| Formula | name | string | `teton` in tap `atelier-fashion/homebrew-tap` (`Formula/teton.rb`) |
| Formula | version | string | MUST equal the Release tag it points at (BR-3/BR-4) |
| Formula | service | block | runs `tetond` with keep-alive; log paths under brew var (BR-6) |
| LandingPage | url | string | `https://tetoncode.ai`, HTTPS required |
| LandingPage | install_command | string | MUST match the current formula invocation (BR-8) |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| release_tagged | maintainer pushes `vX.Y.Z` tag | tag, commit |
| artifacts_published | release workflow finishes per-target builds + smoke checks | tarball URLs, sha256s |
| formula_bumped | automated commit to the tap after artifacts publish | version, per-target URLs + sha256s |
| site_deployed | landing-page deploy (push to site source or release) | current version + install command |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| Push a release tag | maintainers only |
| Push to homebrew-tap | release workflow (scoped token) and maintainers |
| Modify tetoncode.ai DNS | domain owner |

## Business Rules

- [ ] BR-1: `brew install atelier-fashion/tap/teton` is the **complete**
      install on a supported machine: no Rust toolchain, no cmake, no second
      command, no feature-flag knowledge. It installs both `teton` and
      `tetond`.
- [ ] BR-2: Release binaries are built **with** `tetond/llama` — a formula
      install must never reproduce the loaderless state
      ("installed and verified … no local inference engine"). macOS arm64
      carries Metal; x86_64 targets are CPU-only and the probe reports that
      honestly rather than the page or binary overclaiming.
- [ ] BR-3: One version, three places, mechanically agreed: the git tag, the
      workspace `Cargo.toml` version, and the formula version. The release
      workflow verifies tag == Cargo version **before building** and fails
      loudly on mismatch, with a distinct failure classification — never a
      generic exit that reads as a build flake (informed by LESSON-442).
- [ ] BR-4: The formula bump is automated and atomic with the release: a
      release is not complete until the tap formula points at the new
      artifacts with their real sha256s. A bump failure fails the release
      workflow loudly; it must never leave a published release with a stale
      formula silently in place (informed by LESSON-447 — a degraded path must
      preserve the invariant and be visible).
- [ ] BR-5: Formula sha256s are computed from the actually-uploaded artifact
      bytes in the same workflow run — never hand-typed, never copied from a
      local build. `brew` then re-verifies them at install time on every user
      machine (mirrors the REQ-547 BR-6/BR-8 posture: integrity claims are
      mechanically derived and mechanically checked).
- [ ] BR-6: `brew services start teton` runs `tetond` under launchd with
      keep-alive; `stop`/`restart` behave; daemon stdout/stderr land under
      brew's standard var/log location so "where are the logs" has a brew
      answer.
- [ ] BR-7: Cross-platform claims are verified per target, never extrapolated
      from one OS (informed by LESSON-433): the release workflow smoke-tests
      each tarball — binaries execute, `teton --version` reports the tag,
      `tetond` starts and answers a handshake — before the formula bump
      publishes any of them. Native runners for arm64 macOS and x86_64
      Linux; x86_64 macOS runs under Rosetta 2 on an arm64 runner (GitHub
      retired Intel macOS runners), and the release notes record that leg as
      Rosetta-verified, not native-verified — the LESSON-433 rule applied to
      its own verification story.
- [ ] BR-8: The landing page's install command and displayed version derive
      from the release source of truth (release metadata or tap formula), not
      hand-edited copy that can drift from reality.
- [ ] BR-9: The release workflow asserts the DECISION 3 refusal on the actual
      release binary: running the built `tetond` with `TETON_TEST_SEAMS=1`
      must refuse to start. This pins "shipped binaries cannot be steered by
      test seams" as a release gate, not a code comment.
- [ ] BR-10: The page never claims what the shipped binaries cannot do:
      Linux is CPU-only in v1, Windows is unsupported, and the local model
      downloads on first run with consent (size named) — the honesty posture
      of the consent flow extends to marketing copy.

## Acceptance Criteria

- [ ] AC-1: On a clean macOS Apple Silicon machine with only Homebrew
      installed: `brew install atelier-fashion/tap/teton`, then
      `brew services start teton`, then `teton` reaches the first-run model
      proposal (names the pick, download size, RAM floor). Human-verified and
      signed off, mirroring the AC-13 runbook posture.
- [ ] AC-2: The same sequence works on macOS x86_64 and Linux x86_64
      (Homebrew on Linux), each verified on its own platform — a green
      arm64 run is not evidence for the others (informed by LESSON-433).
      CI smoke per platform is mandatory; human sign-off per platform is
      recorded when hardware is available, and unrun legs are recorded as
      unrun, not assumed.
- [x] AC-3: Pushing tag `vX.Y.Z` runs one workflow that: refuses on
      tag/Cargo-version mismatch before building; builds all three targets
      with `tetond/llama`; smoke-tests each on its own platform (BR-7);
      asserts the seam refusal (BR-9); publishes the GitHub Release with
      tarballs + checksums; and bumps the tap formula — all green in one run
      with no manual step.
- [ ] AC-4: `brew upgrade teton` from release N-1 to N works and the running
      daemon story is documented (upgrade does not silently leave an old
      `tetond` running — `brew services restart` guidance or automation).
      Mechanically satisfiable only from the second release onward; for
      v0.1.0 this criterion is staged (the upgrade path exists and is
      documented) and first exercised by the v0.1.x follow-up release.
- [ ] AC-5: The formula's `test do` block passes in CI for the tap
      (`teton --version` matches the formula version).
- [ ] AC-6: `brew services start|stop|restart teton` manage the daemon;
      after `start`, `teton doctor` reports a healthy socket without any
      manual `tetond` invocation.
- [x] AC-7: `https://tetoncode.ai` serves the landing page over HTTPS with
      the overview and the install command; the displayed version matches the
      latest release at deploy time (BR-8).
- [x] AC-8: README quick-start is replaced with the brew one-liner (source
      build remains documented for contributors), and the stale
      "pre-alpha — product spec stage" status line is corrected.

## External Dependencies

- GitHub: rights to create `atelier-fashion/homebrew-tap`; a scoped token
  (fine-grained PAT or deploy key) for the release workflow to push formula
  bumps; macOS arm64, macOS x86_64, and ubuntu runners (all standard GitHub
  Actions hosted runners; cmake preinstalled).
- DNS control for `tetoncode.ai` (GoDaddy, nameservers `domaincontrol.com` —
  Open Question 1, resolved 2026-08-01).
- Site hosting: the existing Atelier Google Cloud infrastructure
  (user-confirmed); needs the GCP project id, deploy service account /
  workload-identity wiring for CI, and TLS cert provisioning for
  `tetoncode.ai` on whichever surface the Atelier site uses (OQ-5).
- No new Rust dependencies.

## Assumptions

- Ad-hoc code signing (what the arm64 linker already emits) is sufficient for
  brew-distributed CLI binaries in v1 — no Apple Developer ID / notarization.
  Holds as long as binaries are only distributed through brew, not direct
  download links on the site. **Unconfirmed by the user** (asked, unanswered);
  cheap to revisit before implementation.
- Linux x86_64 (CPU-only inference) is in scope for v1. **Unconfirmed by the
  user** (asked, unanswered); dropping it only shrinks the target matrix.
- The landing page is hosted on the existing Atelier Google Cloud
  infrastructure (**user-confirmed 2026-07-24** — same infra as the Atelier
  website). The specific GCP surface (Cloud Run vs Cloud Storage + LB/CDN vs
  Firebase Hosting) follows whatever the Atelier site already uses — resolved
  at architecture time (OQ-5); a static page fits any of them.
- The first public tag is `v0.1.0` from the current workspace version.
- The tap repo is named `atelier-fashion/homebrew-tap` (formula invocation
  `atelier-fashion/tap/teton`), keeping room for future formulas.
- GitHub-hosted macOS arm64 runners can build llama.cpp with Metal support
  (kernels compile; execution on the user's machine). If runner hardware
  proves incompatible, the fallback is building Metal support without
  runtime GPU verification in CI plus the AC-1 human leg.

## Open Questions

- [x] OQ-1: Where is DNS for `tetoncode.ai` managed today (registrar,
      nameservers)? Blocks AC-7's final hookup, nothing else.
      **Resolved 2026-08-01:** registrar is GoDaddy; nameservers are
      `domaincontrol.com` (GoDaddy's default DNS).
- [ ] OQ-2: Should `brew services` support on Linux (systemd user units) be a
      v1 claim, or is Linux v1 "binaries install; run `tetond` yourself /
      write your own unit"? Recommend the latter, documented honestly (BR-10).
- [ ] OQ-3: Version cadence and changelog discipline — tag-driven releases
      imply release notes; generated from commit log, or hand-written
      highlights?
- [ ] OQ-4: Does the landing page carry any download links besides the brew
      command? If yes, the ad-hoc signing assumption breaks (Gatekeeper
      quarantines browser downloads) and notarization enters scope.
- [x] OQ-5: Which GCP surface serves the Atelier website (Cloud Run, Cloud
      Storage + LB/CDN, Firebase Hosting), and under which project id? The
      landing page follows the same pattern; needed at architecture time,
      along with CI deploy credentials (service account or workload
      identity).
      **Resolved 2026-08-01** (per `docs/site-deploy-runbook.md` §1 intake):
      - Surface: `gcs` — Cloud Storage behind an external HTTPS load
        balancer with Cloud CDN, using the runbook §4 resource names
        (bucket `tetoncode-ai`, URL map `teton-site-lb`, managed cert
        `teton-site-cert`) plus a reserved global IP.
      - Project: a dedicated new GCP project, not a shared Atelier one.
        Its id is deliberately not written here — ADR-548-3 keeps it out
        of this public repository; the authoritative copy is the repo
        secret `GCP_PROJECT` (and the GCP console).
      - CI deploy identity: workload identity federation as the service
        account held in the repo secret `GCP_SERVICE_ACCOUNT`; the
        provider's attribute condition was restricted at creation time to
        `atelier-fashion/teton-code` and refs `refs/heads/main` /
        `refs/tags/v*`, so runbook §9's REQUIRED-HUMAN item is already done.
      - Repo config set: secrets `GCP_PROJECT`, `GCP_WIF_PROVIDER`,
        `GCP_SERVICE_ACCOUNT`; vars `GCP_DEPLOY_SURFACE=gcs`,
        `GCP_SITE_BUCKET=tetoncode-ai`, `GCP_CDN_URL_MAP=teton-site-lb`.
      - Lesson from the first configured deploy run: the deploy service
        account needs `roles/storage.legacyBucketReader` on the bucket in
        addition to the runbook's `roles/storage.objectAdmin`, because
        `gcloud storage rsync` calls `storage.buckets.get`. The run failed
        without it; the runbook §3 roles table now records both.

## Deferred (verify pass, 2026-07-25)

Two security findings from the Phase-5 audit were deliberately NOT implemented
in this REQ, because both require repo-admin action in the GitHub UI that the
pipeline cannot perform or verify. Neither blocks the one-command install; both
harden the supply chain around it and should be done before the project has
real users.

- **Build provenance / artifact signing.** — **now REQ-550**
  (`signed-releases-and-build-provenance`, spec approved, in flight). `checksums.txt` is published beside
  the assets it describes, over the same channel, mutable by the same
  principals — a direct-download user verifies only that the release page
  agrees with itself. (Homebrew users are covered: the tap's pinned `sha256`
  lives in a different repo behind a different token.) Fix is
  `actions/attest-build-provenance` in the release job plus
  `gh attestation verify` in the runbook, needing `id-token: write` +
  `attestations: write`.
- **Environment-gated secrets.** — folded into REQ-550, whose secret
  inventory verified only `HOMEBREW_TAP_TOKEN` is repo-scoped and created the
  environments. `HOMEBREW_TAP_TOKEN` and the `GCP_*` secrets
  are repository secrets, readable by any workflow on any ref. The tap token is
  the highest-value credential in the design — it rewrites the formula every
  `brew install` executes. Fix is GitHub Environments (`tap-publish`,
  `site-deploy`) with deployment branch/tag rules limited to `v*.*.*` and
  `main`, plus `assertion.ref` in the WIF attribute condition.

## Out of Scope

- homebrew-core submission (requires notability thresholds; revisit
  post-traction).
- CUDA-enabled Linux builds (needs CUDA toolkit in CI and runtime detection;
  separate REQ when Linux GPU users materialize).
- Windows support, winget/scoop manifests.
- Publishing to crates.io / `cargo install` distribution.
- Direct-download binaries on the website (ties to OQ-4 / notarization).
- A docs site, blog, or anything at tetoncode.ai beyond the single landing
  page.
- VS Code extension distribution (phase 2 per the charter).
- Auto-update inside `teton`/`tetond` (brew upgrade is the update channel).

## Retrieved Context

- LESSON-433 (lesson, score 6): Single-platform local verification gives false confidence
- LESSON-441 (lesson, score 5): A fix pass is new code — re-verify adversarially
- LESSON-443 (lesson, score 5): A guard keyed on a feature's absence disables itself
- LESSON-444 (lesson, score 5): A C library's assert is a process abort
- LESSON-445 (lesson, score 5): Stage, then commit only after re-checking authority
- LESSON-442 (lesson, score 3): An uncaught exception's exit code can collide with a meaningful one
- LESSON-446 (lesson, score 3): Token budgets must share a currency
- LESSON-447 (lesson, score 3): A fallback must preserve the guarded invariant
- LESSON-448 (lesson, score 3): Test-double speed masks executor blocking
- LESSON-449 (lesson, score 3): Compose intents when rebasing parallel fixes
- LESSON-450 (lesson, score 3): Sync e2e on state, not the event
- LESSON-451 (lesson, score 3): Seams share the production commit path
- LESSON-452 (lesson, score 3): Decoder lifetime must match stream lifetime
- LESSON-453 (lesson, score 3): Verify callee buffer contracts
- LESSON-432 (lesson, score 3): Provenance from files touched, not arg name

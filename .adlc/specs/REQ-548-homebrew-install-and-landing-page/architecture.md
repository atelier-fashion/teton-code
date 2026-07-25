# REQ-548 Architecture — One-command Homebrew install + tetoncode.ai landing page

## Approach

Three legs, one source of truth. Everything version-shaped derives from the git
tag, which the release workflow verifies equals the workspace `Cargo.toml`
version before any build starts.

```
 push tag vX.Y.Z
      │
      ▼
 ┌─────────────┐   mismatch → exit 64 + ::error (never a generic 1)
 │  preflight  │──────────────────────────────────────────────► FAIL LOUD
 │ tag==Cargo  │
 └──────┬──────┘
        ▼
 ┌────────────────────────────────────────────────┐
 │ build matrix (all with --features tetond/llama)│
 │  aarch64-apple-darwin   macos-15 (native)      │
 │  x86_64-apple-darwin    macos-15 (cross-built, │
 │                         smoke under Rosetta 2) │
 │  x86_64-unknown-linux-gnu ubuntu-24.04 (native)│
 │  each: build → tarball → SMOKE → sha256        │
 └──────┬─────────────────────────────────────────┘
        ▼
 ┌─────────────┐    ┌──────────────┐    ┌───────────────────┐
 │ GitHub      │───►│ bump-formula │───►│ deploy-site       │
 │ Release     │    │ render tmpl, │    │ (DISPATCHED by    │
 │ + checksums │    │ push to tap  │    │  release.yml once │
 └─────────────┘    └──────────────┘    │  the tap bump is  │
                                        │  green — never    │
                                        │  before it, never │
                                        │  beside it;       │
                                        │  fails loud until │
                                        │  GCP secrets set) │
                                        └───────────────────┘
```

The `bump-formula ──► deploy-site` arrow is an explicit `workflow_dispatch`,
not an event subscription, and its direction is an invariant — see ADR-548-3
and ADR-548-3a.

The smoke gate per target (BR-7/BR-9), derived from integration probing:

- `teton --version` and `tetond --version` both report the tag's version
  (clap `version` attr / `env!("CARGO_PKG_VERSION")` — both verified present).
- `TETON_TEST_SEAMS=1 ./tetond` exits **non-zero** with the refusal text —
  the DECISION 3 panic fires unconditionally during `DaemonRuntime::from_env`
  (`load_catalog` consults `test_seams_enabled()` at startup), so this is a
  reliable one-line assertion on the actual shipped binary (BR-9).
- Handshake: start `tetond` backgrounded with `XDG_RUNTIME_DIR` pointed at a
  scratch dir, run `teton doctor`, and assert on doctor's **output text**
  (daemon version line) — NOT its exit code, because `doctor` deliberately
  exits 0 when the daemon is unreachable (non-destructive by design,
  `crates/teton/src/main.rs` doctor path).

## Key facts the design leans on (from codebase exploration)

- `tetond` runs **foreground**, no fork — exactly what launchd/`brew services`
  wants. Second-instance startup exits 0 with "already running" (flock guard),
  so a brew-services restart race is benign.
- launchd-launched `tetond` (no `XDG_RUNTIME_DIR`) and a terminal-launched
  `teton` resolve the **same** base dir on macOS
  (`~/Library/Application Support/teton`, `socket_path.rs` precedence) — the
  service and the CLI meet at the same socket with zero configuration.
- `Cargo.toml` already carries a tuned `[profile.release]` (lto, strip) — the
  tarballs ship what `--release` produces; no profile changes needed.
- CI precedent to follow: `ci.yml`'s runner labels, `Swatinem/rust-cache@v2`,
  `CARGO_TERM_COLOR`, per-ref concurrency, and the `tools/refresh-catalog.py`
  exit-code taxonomy (0 / distinct-failure / 75-unverified) with `::error` /
  `::warning` annotations (LESSON-442).
- **And to extend.** Verify found this REQ shipping a release pipeline with no
  standing CI at all: every workflow and every script under `tools/release/`
  was first exercised by cutting a tag, which is after they can still be wrong.
  `ci.yml`'s `tooling` job closes that on every PR — `actionlint`
  (shellcheck-backed) over `.github/workflows/*.yml`, `shellcheck` over
  `tools/release/*.sh` + `site/render.sh`, and `tools/release/selftest.sh`,
  which drives the release scripts through their success *and* failure paths
  with no network and no cargo. `tools/release/selftest.sh` is a contract path:
  the CI job, the runbook, and the release pipeline all name it.

## ADRs

### ADR-548-1: Distribution is a prebuilt-binary tap, formula source-of-truth lives in this repo

**Decision**: ship via `atelier-fashion/homebrew-tap` with per-target tarballs
from GitHub Releases. The formula is **rendered from a template in this repo**
(`packaging/homebrew/teton.rb.tmpl`); the release workflow's `bump-formula`
job renders it with the tag + computed sha256s and pushes to the tap. The tap
repo is a publish target, never hand-edited.

**Rationale**: source-build formulas reimpose the Rust+cmake burden the REQ
removes; homebrew-core needs notability we don't have. Keeping the template
here means formula changes ride this repo's PR review flow, and the tap can be
regenerated from scratch at any release (BR-4's atomicity has one writer).

**Alternatives rejected**: cargo-dist (optimized for single-package bins; our
two-crate daemon+CLI pair and custom `service` block fight its model);
hand-maintained formula in the tap (drift by construction, violates BR-4).

### ADR-548-2: x86_64-apple-darwin is cross-compiled and Rosetta-smoked

**Decision**: build `x86_64-apple-darwin` on the arm64 macOS runner
(`rustup target add` + llama.cpp via `CMAKE_OSX_ARCHITECTURES=x86_64`), smoke
it under Rosetta 2 on the same runner, and record that leg as
**Rosetta-verified** in release notes.

**Rationale**: GitHub retired Intel macOS runners. Rosetta execution proves
the binary loads and runs its CPU paths; it is not native-hardware evidence,
and per LESSON-433 the claim is recorded at exactly its strength (spec BR-7).

### ADR-548-3: The site deploy is dispatched by the release run, version-injected, and loudly degraded until GCP secrets exist

**Decision**: `site/` holds a dependency-free static page; a small render step
injects the current version + install command from the release tag at deploy
time (BR-8 — no hand-edited version strings). `deploy-site.yml` targets the
Atelier GCP infra (user-confirmed hosting). Until the GCP secrets (project id,
workload identity / SA) are configured — OQ-5 — the deploy job **fails its
final step with a ::warning-annotated, named reason** ("site deploy blocked:
required GCP configuration is not set in this repository") rather than silently
succeeding while deploying nothing (LESSON-447: a degraded path must be visible
and preserve the invariant — here, "green deploy job ⇒ site actually
deployed").

**How it is triggered — corrected during verify.** The original decision said
"runs on release-published and `workflow_dispatch`". As implemented, that
subscription can *never* fire: `release.yml` creates the release with
`gh release create` under the default `GITHUB_TOKEN`, and GitHub deliberately
does not raise workflow-triggering events for actions taken by that token. The
site deploy would have sat silent after every release — the exact
green-while-nothing-happened failure the rest of this ADR is built to refuse,
reintroduced by the trigger itself.

Implemented instead: **`release.yml` dispatches `deploy-site.yml` explicitly,
after `bump-formula` succeeds.** The dispatched run resolves its version from
the repository's latest published release, so the page can still only ever
advertise a version that exists (BR-8) and the dispatcher needs no input.
`deploy-site.yml` keeps the `release: published` trigger as a fallback for a
release cut by a human or a PAT — those *do* raise the event — but nothing in
the pipeline depends on it.

**The guard is receipt-based, not status-based.** Each deploy step writes a
receipt file as its *last* action, and the final `Deploy result` step passes
only on a non-empty receipt. Every other outcome — no GCP configuration, an
unrecognised surface, a deploy that died halfway — exits `78` (`EX_CONFIG`)
with the reason named. A receipt cannot be written by a step that did not
complete, which is what makes "this job is green" mean "tetoncode.ai was
republished" rather than "no step reported an error". The guard also
distinguishes *"the configuration step never ran"* (an earlier step failed —
defer to it) from *"it ran and found nothing"*, so an upstream render or
version failure is never reported as a missing-secret problem.

**Rationale**: decouples the shippable, reviewable site + automation from the
one input only the user can provide (which GCP surface/project the Atelier
site uses). First real deploy is a human-confirmed step per the infra rule.

### ADR-548-3a (addendum): the site deploy must never race or precede the tap bump

**Decision**: the site deploy is ordered strictly *after* `bump-formula`
succeeds, and is dispatched by it rather than running concurrently with it.

**Rationale**: the landing page's whole job is to display a version and the
`brew install` command that fetches it. Those two claims are only
simultaneously true once the tap points at the new release. A site deploy that
raced the bump — or that fired off the release event in parallel with it, which
is what a plain `release: published` subscription would have done had it worked
— publishes a page advertising vX.Y.Z next to an install command that hands the
visitor vX.Y.Z-1. That is worse than a stale page, because it is confidently
wrong at the moment of peak attention, and no one operating either workflow
would see it: both jobs are green.

**Consequences for anyone editing either workflow.** `deploy-site.yml` must not
regain a trigger that fires on release publication from the pipeline's own
releases, and `release.yml` must not move its dispatch above or alongside
`bump-formula`. If `bump-formula` fails, the site deploy must not run at all:
the previous page, advertising the previous version, is *correct* for a tap that
still points at the previous version. `deploy-site.yml`'s
`concurrency: deploy-site` group with `cancel-in-progress: false` backstops the
ordering if two runs are ever in flight — they queue, they do not interleave.

### ADR-548-4: Version preflight uses a distinct exit code

**Decision**: `tools/release/verify-version.sh` exits `0` (match) or `64`
(EX_USAGE — tag/Cargo mismatch), never a bare `1`, and the workflow maps 64 to
a named `::error`. Mirrors the catalog checker's taxonomy so an infrastructure
failure can never masquerade as a version mismatch or vice versa (LESSON-442).

## Data model / API changes

None in the product. No protocol, daemon, or CLI code changes — the release
pipeline consumes existing `--version` surfaces and the existing seam refusal.
(`.gitignore` gains site render output; README gains the brew quick-start.)

## Proposed additions to `.adlc/context/architecture.md`

After merge, add an ADR-006-style entry summarizing ADR-548-1 (distribution
channel: tap + prebuilt binaries, formula template in-repo) — it is a
project-level decision future REQs (auto-update, winget, homebrew-core) build
on. Deferred to wrapup so the context doc references the merged reality.

## Task graph

```
Tier 1:  TASK-015 (release workflow + scripts)     TASK-018 (landing page)
              │                                         │
Tier 2:  TASK-016 (formula template + bump job)    TASK-019 (site deploy wf + runbook)
              │
Tier 3:  TASK-017 (tap bootstrap)                  TASK-020 (README + release runbook)
                                                    [depends on TASK-016]
```

Six tasks, max fan-in 1, two independent chains — Tier 1 pair runs in
parallel, then Tier 2 pair, then Tier 3 pair (LESSON-449 noted: single
session, one branch, no parallel-PR composition risk inside this REQ).

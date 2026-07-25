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
 │ GitHub      │───►│ bump-formula │    │ deploy-site       │
 │ Release     │    │ render tmpl, │    │ (on release       │
 │ + checksums │    │ push to tap  │    │  published;       │
 └─────────────┘    └──────────────┘    │  loud no-op until │
                                        │  GCP secrets set) │
                                        └───────────────────┘
```

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

### ADR-548-3: The site deploy is release-triggered, version-injected, and loudly degraded until GCP secrets exist

**Decision**: `site/` holds a dependency-free static page; a small render step
injects the current version + install command from the release tag at deploy
time (BR-8 — no hand-edited version strings). `deploy-site.yml` runs on
release-published and `workflow_dispatch`, targeting the Atelier GCP infra
(user-confirmed hosting). Until the GCP secrets (project id, workload identity
/ SA) are configured — OQ-5 — the deploy job **fails its final step with a
::warning-annotated, named reason** ("site deploy skipped: GCP secrets not
configured") rather than silently succeeding while deploying nothing
(LESSON-447: a degraded path must be visible and preserve the invariant —
here, "green deploy job ⇒ site actually deployed").

**Rationale**: decouples the shippable, reviewable site + automation from the
one input only the user can provide (which GCP surface/project the Atelier
site uses). First real deploy is a human-confirmed step per the infra rule.

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

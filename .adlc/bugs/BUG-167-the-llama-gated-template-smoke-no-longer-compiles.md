---
id: BUG-167
title: "The llama-gated template smoke no longer compiles"
status: resolved
severity: medium
created: 2026-08-14
updated: 2026-08-14
component: "daemon"
domain: "harness"
stack: ["rust", "daemon", "cargo", "ci"]
concerns: ["testing", "reliability", "developer-experience"]
tags: ["llama", "feature-gate", "compile-drift", "template-smoke", "req-554", "req-564", "ci-blind-spot", "ci-coverage", "bug-164"]
---

> **Registry note.** This defect was found and fixed twice in parallel
> sessions: PR #127 (this record, filed as BUG-166 and renumbered in flight)
> and PR #129 (an identical call-site fix plus the CI class guard, whose
> record was filed as a second `BUG-166` and merged five seconds after #127).
> The two records are consolidated here; `BUG-166` uniquely names the
> rejection-notice bug from PR #128.

## Description

REQ-564 (PR #81) added a `SessionId` parameter to `LocalEngineSource::new` —
the prefix cache's key (BR-3) — and updated every call site the compiler could
see. `crates/tetond/tests/template_smoke.rs` is `#![cfg(feature = "llama")]`,
so it was outside the compiler's sight for that change and for every API pass
since (REQ-571 among them), and kept passing two arguments. The ungated
`conversation_carry.rs` constructs the same source with three arguments; only
the gated target could drift.

Default (no-llama) CI never compiled this target — that was the point of the
gate, since compiling it means building llama.cpp — so the break was invisible
to every automated leg. The breakage surfaced only under
`cargo clippy --workspace --all-targets --all-features`, or when someone ran
the smoke the way its own doc header says to — which is exactly the moment the
weights-backed evidence is wanted and exactly the moment it turns out to be
unrunnable. This is the compile-time member of the BUG-164 family: llama-gated
suites are manual gates, and a manual gate can rot between the moments someone
runs it.

The consequence is not user-facing. It is that REQ-554 AC-6's acceptance
vehicle — the one real-weights check that the ChatML template path emits a
well-formed tool call (mock-only green is not acceptance, LESSON-433) — could
not even be built.

## Reproduction Steps

1. Check out `origin/main` at `d093ede` (or any commit since PR #81).
2. `cargo build --release -p tetond --features llama --test template_smoke`
3. E0061 at `tests/template_smoke.rs:82`: `LocalEngineSource::new(engine,
   format)` is missing the `SessionId` argument.

## Expected Behavior

Every target in the workspace compiles under every supported feature
combination, whether or not any CI leg happens to exercise it. The gated smoke
compiles; only *running* it needs weights (`TETON_TEST_GGUF`), per its
`#[ignore]` contract.

## Actual Behavior

The `template_smoke` test target failed to compile with `--features llama`
from the REQ-564 merge (PR #81, 2026-08-10) until PR #127/#129 landed — the
breakage shipped in the 0.1.14 tag.

## Environment

- Platform: all (compile-time)
- Version: broken since PR #81 (post-0.1.13); present in the 0.1.14 tag
- Found independently twice on 2026-08-14: during REQ-572's review
  (pre-existing there, not that branch's regression) and by a manual
  `--features llama` build of the smoke.
- Affected: any `--features llama` build of the `template_smoke` target.
  Default builds and CI were unaffected — which is precisely the defect's
  cover.

## Root Cause

Two facts compose:

1. **The compiler's "all call sites updated" verdict is scoped to the features
   it compiled with.** `--all-targets` expands only the targets the *active
   feature set* admits, so a `cfg`'d-out target is not checked and a
   shared-API change can be complete for the always-on surface and silently
   incomplete for the gated one — "every caller was updated" silently meant
   "every caller the compiler was shown".
2. **No automated leg compiled the gated surface.** CI's clippy leg
   (`--workspace --all-targets`, warnings denied) made the same
   default-features pass, so nothing on `main` ever asserted the gated target
   still builds.

BUG-164 documented the runtime flavor of this blind spot (a gated e2e can
exercise a stale daemon); this is the compile flavor (a gated target stops
building at all). The gap was structural, not a one-off typo: any gated target
(`llama`, `presence`, `live`, `test-seam`) rots the same way the next time an
API it uses moves.

## Resolution

Two parts — the instance and the class:

- **The call** (`template_smoke.rs`, PR #127 and PR #129 identically): pass
  the session id the way the daemon's engine slot does —
  `SessionId::from("template-smoke")` — with a comment noting the id is the
  prefix cache's key (REQ-564 BR-3) and that a fixed id is faithful for a
  one-turn, one-session smoke. The smoke's intent (one templated turn, AC-6's
  fidelity claim) is untouched.
- **The class** (`.github/workflows/ci.yml`, PR #129): a new `gated` job runs
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` on
  `macos-latest`. Compiling is cheap where running is not: the gated tests
  need weights or live keys to *run* but only cmake to *build*, so the leg
  buys "every target still compiles, at the standing lint bar" with no
  secrets and no downloads. macOS deliberately — `presence` is cfg-gated to
  `target_os = "macos"`, so an ubuntu leg with `--all-features` would still
  strip the LocalAuthentication FFI and claim more than it checked, the same
  trap the job exists to close.

PR #127 also swept the rest of the gated surface by hand
(`tetond/tests/template_smoke.rs`, `teton-inference/tests/llama_smoke.rs`,
and the gated source in `runtime.rs`, `lib.rs`, `engine.rs`): only
`template_smoke` had drifted. The `gated` job now performs that sweep on every
push.

## Verification

- Pre-fix, both surfacing commands reproduce E0061:
  `cargo check -p tetond --features llama --test template_smoke` (confirmed on
  the unmodified tree via stash — the new gate demonstrably covers the target
  it was added for) and the release build of the same target.
- Post-fix: `cargo clippy --workspace --all-targets --all-features -- -D
  warnings` exits 0 on macOS (llama.cpp compiled, `presence` FFI expanded) —
  the exact command the new CI leg runs; its first run on `main` (`51cf474`)
  passed. The exact repro,
  `cargo build --release -p tetond --features llama --test template_smoke`,
  exits 0.
- `cargo fmt --all -- --check` clean.
- The smoke itself still requires local weights to execute and remains
  `#[ignore]`d; nothing about its runtime contract changed. The next
  real-weights run is unblocked.

## Lessons Captured

- `LESSON-515` — a feature-gated target is invisible to every refactor: the
  compiler's completeness verdict is scoped to the features compiled with, so
  gated targets need an automated all-features leg (or an explicit sweep)
  after any shared-API change.

## Files Changed

- `crates/tetond/tests/template_smoke.rs` — import `SessionId`, pass it at the
  `LocalEngineSource::new` call site (PR #127; PR #129 added the explanatory
  comment).
- `.github/workflows/ci.yml` — new `gated` job: all-features clippy on macOS
  so feature-gated targets cannot rot unseen again (PR #129).
- `.adlc/knowledge/lessons/LESSON-515-a-feature-gated-target-is-invisible-to-refactors.md`
  — new (PR #127).
- `.adlc/bugs/BUG-166-a-gated-test-target-no-ci-leg-compiles.md` — PR #129's
  record of this same defect; consolidated into this file and removed.

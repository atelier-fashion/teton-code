---
id: BUG-166
title: "A feature-gated test target that no CI leg compiles rotted on main"
status: resolved
severity: medium
created: 2026-08-14
updated: 2026-08-14
component: "daemon/tests"
domain: "harness"
stack: ["rust", "cargo", "ci"]
concerns: ["testing", "reliability", "developer-experience"]
tags: ["template-smoke", "req-554", "req-564", "feature-gate", "llama", "ci-coverage", "lesson-510"]
---

## Description

`crates/tetond/tests/template_smoke.rs` — the REQ-554 AC-6 smoke, the one test
that drives a templated turn through a **real** engine on real weights — does
not compile on `main`. Line 82 calls `LocalEngineSource::new(engine, format)`
with two arguments; REQ-564 (BR-3) widened the constructor to three, adding
`session_id: SessionId` as the prefix cache's key, and updated every caller the
compiler could see. This one it could not: the whole file is
`#![cfg(feature = "llama")]`, `llama` is non-default, and no CI leg enables it —
so the two-arg call has sat broken since REQ-564 landed (PR #81), with CI green
the whole time.

The breakage surfaces only under
`cargo clippy --workspace --all-targets --all-features` — or when someone runs
the smoke the way its own doc header says to, which is exactly the moment the
weights-backed evidence is wanted and exactly the moment it turns out to be
unrunnable.

## Reproduction Steps

1. On `main` at `07eb97c` (or any commit since PR #81):
   `cargo check -p tetond --features llama --test template_smoke`

## Expected Behavior

The gated smoke compiles; only *running* it needs weights
(`TETON_TEST_GGUF`), per its `#[ignore]` contract.

## Actual Behavior

```text
error[E0061]: this function takes 3 arguments but 2 arguments were supplied
  --> crates/tetond/tests/template_smoke.rs:82:22
```

## Environment

- Platform: all (compile-time)
- Version: broken since REQ-564 landed (PR #81, post-0.1.13); shipped in the
  0.1.14 tag. Found during REQ-572's review — pre-existing there, not that
  branch's regression.

## Root Cause

Two gates each checked a property adjacent to the one that mattered:

- The refactor's safety net was "the workspace compiles" — but
  `--all-targets` expands only the targets the *active feature set* admits.
  A `#![cfg(feature = "llama")]` test target is invisible to a
  default-features build, so "every caller was updated" silently meant "every
  caller the compiler was shown".
- CI's clippy leg (`--workspace --all-targets`, warnings denied) makes the
  same default-features pass, so nothing on `main` ever asserted the gated
  target still builds.

This is LESSON-510's shape one layer up: existence is not freshness. The file
existed, the suite was green, and the property nobody checked — "this target
still compiles against today's API" — was the one that had failed. The gap is
structural, not a one-off typo: *any* future gated target (`llama`,
`presence`, `live`, `test-seam`) rots the same way the next time an API it
uses moves.

## Resolution

Two parts — the instance and the class:

- **The call** (`template_smoke.rs:82`): pass the session id the way the
  daemon's engine slot does — `SessionId::from("template-smoke")`, with a
  comment noting the id is the prefix cache's key (REQ-564 BR-3) and that a
  fixed id is faithful for a one-turn, one-session smoke. The smoke's intent
  (one templated turn, AC-6's fidelity claim) is untouched.
- **The class** (`.github/workflows/ci.yml`): a new `gated` job runs
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` on
  `macos-latest`. Compiling is cheap where running is not: the gated tests
  need weights or live keys to *run* but only cmake to *build*, so the leg
  buys "every target still compiles, at the standing lint bar" with no
  secrets and no downloads. macOS deliberately — `presence` is cfg-gated to
  `target_os = "macos"`, so an ubuntu leg with `--all-features` would still
  strip the LocalAuthentication FFI and claim more than it checked, the same
  trap the job exists to close.

## Verification

- Pre-fix, the surfacing command reproduces:
  `cargo check -p tetond --features llama --test template_smoke` → E0061
  (confirmed on the unmodified tree via stash — the new gate demonstrably
  covers the target it was added for).
- Post-fix: `cargo clippy --workspace --all-targets --all-features -- -D
  warnings` exits 0 on macOS (llama.cpp compiled, `presence` FFI expanded) —
  the exact command the new CI leg runs.
- `cargo fmt --all -- --check` clean.
- The smoke itself still requires local weights to execute and remains
  `#[ignore]`d; nothing about its runtime contract changed.

## Files Changed

- `crates/tetond/tests/template_smoke.rs` — third `SessionId` argument at the
  REQ-564 constructor.
- `.github/workflows/ci.yml` — new `gated` job: all-features clippy on macOS
  so feature-gated targets cannot rot unseen again.

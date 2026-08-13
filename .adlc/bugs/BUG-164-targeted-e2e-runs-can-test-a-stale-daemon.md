---
id: BUG-164
title: "A targeted e2e run can pass against a stale daemon binary"
status: open
severity: medium
created: 2026-08-13
updated: 2026-08-13
component: "cli"
domain: "harness"
stack: ["rust", "cli", "ci"]
concerns: ["reliability", "developer-experience"]
tags: ["e2e", "test-harness", "stale-binary", "cargo", "false-pass", "verification-integrity"]
---

## Description

`crates/teton/tests/cli_e2e.rs` and `crates/teton/tests/pty_e2e.rs` locate the
`teton-code` daemon by string-joining the filename onto the directory of
`env!("CARGO_BIN_EXE_teton")`. Cargo guarantees a fresh build only for binaries
referenced through a `CARGO_BIN_EXE_*` variable, and `crates/teton/Cargo.toml`
declares no dependency on `tetond`. A targeted run therefore builds `teton` but
not `teton-code`, and the suite executes whatever `teton-code` already sits in
the profile directory — which may predate the change under test by any amount.

The harness already has a `daemon_or_skip()` guard, but it only handles the
daemon being **absent**. A stale-but-present binary passes that guard and is
executed as though it were current. Absence is reported honestly; staleness is
silent.

The consequence is a verification-integrity failure rather than a user-facing
defect: a targeted run can report PASS for a change the daemon does not contain,
so a regression or a mutation can appear survived when it was never exercised.

## Reproduction Steps

1. `cargo build --workspace` (populates `target/debug/teton-code`).
2. Make any observable change to daemon behavior under `crates/tetond/src/`.
3. Run `cargo test -p teton --test cli_e2e`.
4. Cargo rebuilds only `teton`; `target/debug/teton-code` is untouched.
5. The suite passes, exercising the pre-change daemon.

## Expected Behavior

An e2e run either exercises a `teton-code` built from the current working tree,
or refuses to run. It must never execute a stale daemon while reporting success.

## Actual Behavior

The stale daemon is executed and the suite reports PASS.

## Environment

- Platform: macOS (Apple Silicon) and Linux CI
- Version: workspace 0.1.13, toolchain 1.97.1
- Affected: local/targeted runs only. CI is unaffected — `.github/workflows/ci.yml`
  runs `cargo test --workspace`, which builds every binary first.

## Root Cause

Two distinct facts compose:

1. **No build edge.** `teton` does not depend on `tetond`, so nothing in the
   dependency graph obliges Cargo to build `teton-code` when testing `teton`.
   The path is derived by string manipulation, which produces a valid-looking
   path regardless of whether the file behind it is current.
2. **The freshness guard checks the wrong property.** `daemon_or_skip()` tests
   for existence. Existence and freshness are different properties, and the one
   that matters here is untested.

**The originally proposed fix is infeasible.** Switching to
`env!("CARGO_BIN_EXE_teton-code")` cannot work: `CARGO_BIN_EXE_<name>` is set
only for binaries belonging to the *same package* as the integration test.
`teton-code` is a binary of `tetond`, so the variable does not exist when
compiling tests in the `teton` package. Verified empirically — a probe test in
`crates/teton/tests/` fails to compile with:

```
error: environment variable `CARGO_BIN_EXE_teton-code` not defined at compile time
```

This is why `crates/tetond/tests/` can use the variable (same package) and
`crates/teton/tests/` cannot. Any fix must therefore establish freshness by a
mechanism other than the `CARGO_BIN_EXE` edge. Artifact dependencies
(`artifact = "bin"`) would express it directly but require nightly; the pinned
toolchain is stable 1.97.1.

## Resolution

Both suites now resolve the daemon through one shared helper
(`crates/teton/tests/common/mod.rs`) that checks the property that actually
matters. The daemon's mtime is compared against the newest source it is built
from — the `tetond` crate plus the library crates it links (`teton-core`,
`teton-protocol`, `teton-providers`, `teton-inference`), their manifests, and the
workspace `Cargo.toml`/`Cargo.lock`. If the daemon is older, the suite panics
with the offending file named and the exact command to fix it, instead of
running.

Scoping the input set to the daemon's own dependencies is what prevents a false
positive: editing only `crates/teton/src` does not relink `teton-code` under
`--workspace`, and must not read as staleness. The walk fails open — an
unreadable source tree yields no verdict rather than blocking the suite, since
refusing to run over the guard's own bookkeeping would be worse than the
staleness it guards against.

`daemon_or_skip()` is removed from both files. It existed to handle the daemon
being **absent**, which the new check reports with a clearer message; keeping a
second, weaker guard alongside the real one would only invite the two to
disagree.

**Rejected approach — building the daemon from the harness.** The first
implementation shelled out to `cargo build -p tetond --bin teton-code` and took
the executable path from Cargo's JSON artifact message, so a targeted run would
*repair* staleness rather than refuse it. It worked standalone (`cli_e2e` 28/28,
`pty_e2e` 3/3) and the nested build measured as a true no-op (0.07s, zero units
rebuilt), but it reproducibly broke `pty_e2e` when that suite ran after
`cli_e2e` in the same Cargo invocation: the CLI failed to reach the test's daemon
and autostarted its own, hitting the model-consent prompt and timing out at the
20s window. Pre-fix the same command passed 3/3 in 0.34s. The interaction was
reproducible but not explained, and a test harness whose failure mode is not
understood is worse than one that refuses honestly — so the nested build was
dropped in favour of detection, which has no subprocess and no such interaction.

**Not fixable as originally proposed.** See Root Cause: `CARGO_BIN_EXE_teton-code`
is a compile error in this package, verified empirically.

## Verification

A/B against the same command, `cargo test -p teton --test cli_e2e <one test>`,
after touching `crates/tetond/src/main.rs`:

| | pre-fix | post-fix |
|---|---|---|
| Stale daemon | **passed silently** (mtime unchanged: 1786629289 → 1786629289) | refuses, naming `crates/tetond/src/main.rs` |
| After `cargo build --workspace` | passes | passes |

- `cargo test --workspace --no-fail-fast`: **2218 passed, 0 failed, 0 failed targets**.
- `cargo test -p teton --no-fail-fast`: `cli_e2e` 28/28, `pty_e2e` 3/3 in 0.32s —
  matching the pre-fix baseline, confirming the rejected approach's regression is gone.
- `cargo clippy -p teton --all-targets`: clean under workspace `deny` lints.

## Files Changed

- `crates/teton/tests/common/mod.rs` — new. Shared `teton_bin()` / `daemon_bin()`;
  the freshness check and its rationale.
- `crates/teton/tests/cli_e2e.rs` — use the shared helpers; drop the local
  binary-path fns and `daemon_or_skip()`; 24 call sites now take `daemon_bin()`.
- `crates/teton/tests/pty_e2e.rs` — same, 3 call sites.

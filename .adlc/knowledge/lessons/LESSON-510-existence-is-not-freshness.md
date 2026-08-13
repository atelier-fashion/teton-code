---
id: LESSON-510
title: "A harness that checked a binary exists has not checked it is the one under test"
component: "cli"
domain: "harness"
stack: ["rust", "cli", "ci"]
concerns: ["reliability", "testing", "developer-experience"]
tags: ["test-harness", "stale-binary", "cargo", "false-pass", "verification-integrity", "proxy-property"]
req: BUG-164
created: 2026-08-13
updated: 2026-08-13
---

## What Happened

The `teton` crate's e2e suites (`cli_e2e`, `pty_e2e`) located the `teton-code`
daemon by joining the filename onto `env!("CARGO_BIN_EXE_teton")`'s directory,
guarded by a `daemon_or_skip()` helper that checked `.exists()`. Cargo sets
`CARGO_BIN_EXE_<name>` only for binaries of the *same package*, and `teton`
declares no dependency on `tetond`, so nothing obliged Cargo to build the daemon
for a `-p teton` run.

The guard's chosen property — existence — was almost always true, because some
`teton-code` was usually lying in the profile directory from an earlier build. So
a targeted run executed a daemon that could predate the change under test by any
amount and reported PASS. Absence was reported honestly; staleness was silent.

CI never saw it: `cargo test --workspace` builds every binary first. The only
runs that could expose the gap were the local, targeted ones — and those were
exactly the runs that reported success.

## Lesson

A guard reports on the property it checks, not the property it exists for.
`exists()` and "is the artifact under test" are different claims, and when the
cheap proxy diverges from the real one it does so silently, because a proxy is
chosen precisely for being usually-true.

When a harness resolves an artifact it did not itself build, name the property
that makes the run meaningful — freshness, identity, version, digest — and check
*that*. If it cannot be checked, refuse to run. A suite that declines is
recoverable; a suite that passes against the wrong artifact has spent your trust.

Two concrete corollaries:

- **`CARGO_BIN_EXE_<name>` is same-package only.** A cross-package binary cannot
  be reached that way at all — `env!("CARGO_BIN_EXE_other-crate-bin")` is a
  compile error, not a fallback. Cross-crate e2e harnesses therefore have no
  build edge and must establish freshness themselves. Check this before designing
  the harness, not after it silently passes.
- **Prefer refusing to repairing when the repair is not understood.** The first
  fix here shelled out to `cargo build` so a targeted run would *repair*
  staleness. It passed standalone and the nested build measured as a true no-op,
  yet it reproducibly broke the pty suite when that suite ran after the piped one
  in a single Cargo invocation. Reproducible-but-unexplained is not good enough
  for a harness: its whole job is to be the thing you trust when something else
  breaks. Detection has no subprocess and no such interaction.

This is the test-harness form of LESSON-432 (derive a claim from the actual
effect, not from a convenient stand-in) and a sibling of LESSON-504 (a gate's
precondition is part of its claim).

## Why It Matters

It inverts the meaning of a green run, and does so with a bias toward the worst
case: a targeted suite is most likely to be run while iterating on the very
component it does not rebuild, so the daemon is staleest exactly when the
developer most needs the result to be real. A regression or a surviving mutation
reads as "verified". Every downstream judgement built on that run — a mutation
score, a "fix confirmed", a merge — inherits the error, and nothing in the output
hints at it.

The cost is not the debugging time; it is that the suite's verdicts stop being
evidence, retroactively, for every targeted run anyone made.

## Applies When

- A test harness spawns or loads an artifact it did not build: cross-crate e2e
  binaries, prebuilt fixtures, golden files, container images, downloaded models.
- Writing any skip-or-run guard — ask whether the condition checked is the
  condition meant, and what is true when they diverge.
- Reading a targeted (`-p <crate>`, single-file, filtered) run as evidence,
  especially when the full-workspace run is the only one CI performs.
- Choosing between detecting a bad state and automatically repairing it, when the
  repair reaches outside the process.

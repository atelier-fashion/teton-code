---
id: LESSON-433
title: "Single-platform local verification gives false confidence for cross-platform code"
component: "daemon/auth"
domain: "infra"
stack: ["rust", "libc", "ci"]
concerns: ["reliability", "developer-experience"]
tags: ["cross-platform", "cfg-gating", "getpeereid", "so-peercred", "ci", "cross-check"]
req: REQ-544
created: 2026-07-21
updated: 2026-07-21
---

## What Happened

Every "verified green" checkpoint during implementation ran `cargo test` on the
dev machine (macOS/Apple Silicon) only. The socket peer-credential check used
`libc::getpeereid`, a BSD/macOS-only function (glibc does NOT provide it — a code
comment even wrongly claimed it was "portable across macOS and Linux"). It
compiled and passed locally through the entire build and the whole Phase-5 fix
pass, then failed instantly on the Ubuntu CI leg with `E0425: cannot find
function getpeereid`. macOS CI passed; Linux and the acceptance suite (also
Linux) failed.

## Lesson

For code that targets multiple platforms, local single-OS testing proves
nothing about the others — CI-on-every-target is the real gate, and it should
run early, not at merge time. When a fix touches platform APIs (`libc`, sockets,
process control, filesystem modes), `cfg`-gate per platform and, before pushing,
symbol-audit the change (`grep libc::` + confirm each symbol exists on every
target). A cross `cargo check --target <other>` catches it locally IF a cross
C-toolchain is installed (native-dep build scripts like `ring`/`libsqlite3-sys`
need `<target>-gcc`); when it isn't, treat CI as the authoritative check and
don't claim cross-platform "done" off one OS.

## Why It Matters

"All tests pass" on one platform reads as done and is trusted downstream, but
half the target matrix is unverified. The failure is invisible until CI (or a
user on the other OS) hits it, turning a one-line `cfg` fix into a post-merge
scramble. It also erodes trust in every green checkpoint that preceded it.

## Applies When

A Rust (or any compiled) project has a CI matrix spanning OSes; a change touches
`libc`/syscalls/platform-specific APIs; you're about to report a cross-platform
change as verified after building on a single machine; a doc/comment asserts an
API is "portable" — verify that claim against each target's libc, don't inherit
it.

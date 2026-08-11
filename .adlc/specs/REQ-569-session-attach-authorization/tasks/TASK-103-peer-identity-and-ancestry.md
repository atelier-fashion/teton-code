---
id: TASK-103
title: "Peer identity: kernel-attested PID and ancestry, with a platform-free policy layer"
status: draft
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

Obtain the connecting peer's kernel-attested PID and expose "is this peer a
descendant of this daemon?" as a **pure, table-testable decision** over plain
data, with only the syscalls behind a platform seam (ADR-A, ADR-B). No gate is
wired yet — TASK-106 consumes this.

## Files to Create/Modify

- `crates/tetond/src/auth.rs` — extend the peer-credential read to also return the PID. Linux: `SO_PEERCRED` **already fills `ucred.pid`** — it is currently discarded; extract it. macOS/BSD: `getpeereid` cannot supply a pid, so add `getsockopt(SOL_LOCAL, LOCAL_PEERPID)`. Keep the existing uid check and its error taxonomy untouched — this is additive. Return a `PeerIdentity { uid, pid }` (or extend the existing return) rather than a bare u32, so callers cannot mix the two up.
- `crates/tetond/src/peer.rs` — NEW. Two clearly separated halves:
  1. **Platform seam** (`trait ParentOf { fn parent_of(&self, pid: i32) -> Option<i32>; }`): macOS via `sysctl(KERN_PROC_PID)` reading `kp_eproc.e_ppid`; Linux via `/proc/<pid>/status` `PPid:`. Each `#[cfg]`-gated, each tiny.
  2. **Pure policy** (`fn is_descendant_of(peer: i32, ancestor: i32, lookup: &dyn ParentOf, max_depth: usize) -> Ancestry`): walks parent links; returns `Descendant`, `NotDescendant`, or `Indeterminate` (chain broke / depth cap / lookup failed). **`Indeterminate` is a distinct answer, not a synonym for either** — TASK-106 decides its policy; conflating it here would hide a fail-open.
- Unit tests for the pure policy driven by a fake `ParentOf` map (no real processes): direct child, deep chain, not-a-descendant, chain reaching pid 1, a cycle (must terminate), depth-cap exceeded, lookup returning `None` mid-walk.
- One integration test per platform that exercises the REAL `parent_of` on `std::process::id()` and its known parent, `#[cfg]`-gated so both macOS and Linux runners execute their own arm.

## Acceptance Criteria

- [ ] Peer PID is obtained on macOS AND Linux; the existing uid refusal behavior is byte-identical (no regression to `auth.rs`'s error taxonomy).
- [ ] `is_descendant_of` is pure over a `ParentOf` lookup and covered by a table test including the cycle and depth-cap cases (a malicious/looping chain must terminate, never hang).
- [ ] `Indeterminate` is a distinct variant, never silently mapped to "not a descendant".
- [ ] Real-syscall test runs on both platforms (cfg-gated arms, both compiled and executed in CI — LESSON-433: one-platform verification of cfg-gated code is false confidence).
- [ ] `cargo test -p tetond` and `cargo clippy --workspace --all-targets -- -D warnings` green on the developer's platform; CI green on both.

## Technical Notes

- `unsafe` blocks get a `// SAFETY:` comment naming why the pointer/lifetime is valid, matching `auth.rs`'s existing style.
- Depth cap (e.g. 64) exists to bound the walk, not to express policy.
- Do NOT add any signature/keychain machinery — explicitly rejected in ADR-A.
- PID reuse is a known, documented race (ADR-A) — do not attempt to defeat it here.

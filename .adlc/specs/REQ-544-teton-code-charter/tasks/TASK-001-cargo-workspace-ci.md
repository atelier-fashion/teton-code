---
id: TASK-001
title: "Cargo workspace scaffold + CI"
status: draft
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: []
---

## Description

Create the Cargo workspace with all six crates as compilable stubs, plus
GitHub Actions CI (fmt, clippy, test) so every later task lands against a
green pipeline.

## Files to Create/Modify

- `Cargo.toml` — workspace root; members, shared lints, release profile
- `crates/teton-core/src/lib.rs` — stub lib
- `crates/teton-protocol/src/lib.rs` — stub lib
- `crates/teton-providers/src/lib.rs` — stub lib
- `crates/teton-inference/src/lib.rs` — stub lib
- `crates/tetond/src/main.rs` — stub binary (prints version, exits)
- `crates/teton/src/main.rs` — stub binary (prints version, exits)
- `.github/workflows/ci.yml` — fmt --check, clippy -D warnings, cargo test on macOS + Linux runners
- `rust-toolchain.toml` — pinned stable toolchain

## Acceptance Criteria

- [ ] `cargo build --workspace` and `cargo test --workspace` succeed locally
- [ ] CI runs on PRs to main and passes on this scaffold (macOS + ubuntu)
- [ ] `tetond --version` and `teton --version` print the workspace version
- [ ] Crate dependency direction matches REQ-544 architecture.md (binaries depend on libs; libs never on binaries; core has no I/O deps)

## Technical Notes

Plain PR-gated CI (per conventions.md — NOT the staging-first model). Keep
clippy at `-D warnings` from day one; retrofitting is misery. Edition 2021+,
resolver 2.

---
id: TASK-015
title: "Tag-triggered release workflow: version gate, 3-target llama builds, smoke, GitHub Release"
status: complete
parent: REQ-548
created: 2026-07-25
updated: 2026-07-25
dependencies: []
repo: teton-code
---

## Description

The core release pipeline: pushing `vX.Y.Z` runs preflight (tag ==
workspace `Cargo.toml` version, distinct exit code on mismatch — BR-3),
builds `teton` + `tetond` with `--features tetond/llama` for all three
targets, assembles per-target tarballs, smoke-tests each (BR-7/BR-9),
computes sha256s from the actual artifacts (BR-5), and publishes a GitHub
Release with tarballs + `checksums.txt`. A `workflow_dispatch` dry-run input
runs everything except the Release publish, so the pipeline is testable
without burning a tag.

## Files to Create/Modify

- `.github/workflows/release.yml` — the workflow: `preflight` job → 3-leg build matrix → `release` job; follows ci.yml conventions (rust-cache, CARGO_TERM_COLOR, concurrency, ::error/::warning annotations)
- `tools/release/verify-version.sh` — tag vs `Cargo.toml` `[workspace.package] version` check; exit 0 match / 64 mismatch (ADR-548-4), never bare 1
- `tools/release/package.sh` — builds one target with `--features tetond/llama`, assembles `teton-vX.Y.Z-<target>.tar.gz` (teton, tetond, LICENSE, README.md), emits its sha256
- `tools/release/smoke.sh` — extracts a tarball and asserts: both `--version`s equal the tag; `TETON_TEST_SEAMS=1 ./tetond` exits non-zero with the refusal text; backgrounded `tetond` + `teton doctor` output contains the daemon version line (text assertion — doctor exits 0 even when unreachable)

## Acceptance Criteria

- [x] `bash tools/release/verify-version.sh v0.1.0` exits 0 against the current tree; `v9.9.9` exits 64 with a message naming both versions
- [x] Workflow YAML parses (`python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))"`) and `shellcheck`/`bash -n` pass on all three scripts (shellcheck if installed, `bash -n` minimum)
- [x] The build matrix covers exactly `aarch64-apple-darwin` (macos-15), `x86_64-apple-darwin` (cross-compiled on macos-15, smoke under Rosetta 2 — ADR-548-2), `x86_64-unknown-linux-gnu` (ubuntu-24.04); every leg passes `--features tetond/llama`
- [x] `smoke.sh` runs green locally against a `--features tetond/llama` release build of the current tree (arm64 leg), including the seam-refusal and doctor-text assertions
- [x] The `release` job uploads all tarballs plus a single `checksums.txt` whose entries are computed in-workflow from the uploaded files (BR-5) and marks x86_64-darwin as Rosetta-verified in the release notes body
- [x] `workflow_dispatch` with `dry_run=true` runs preflight+build+smoke and skips the Release publish

## Technical Notes

- Preflight reads the version from the root `Cargo.toml` `[workspace.package]`
  block only — do not grep crate manifests (they inherit via `workspace = true`).
- llama.cpp needs cmake — preinstalled on GitHub hosted runners; the x86_64
  cross leg needs `rustup target add x86_64-apple-darwin` and
  `CMAKE_OSX_ARCHITECTURES=x86_64` exported so llama.cpp's cmake build matches
  the Rust target (mismatched arch objects fail at link, not at runtime — fail
  in-job, which is the loud place).
- Smoke's handshake leg sets `XDG_RUNTIME_DIR` to a mktemp dir so the daemon's
  socket/lock/log land in scratch space, and kills the daemon in a trap.
- `tetond --version` prints `tetond X.Y.Z` (manual arg handling in main.rs);
  `teton --version` prints clap's `teton X.Y.Z` — assert with grep on the bare
  version string so both formats pass.
- Release publish via `gh release create` with `--notes` including the
  Rosetta-verified statement; `permissions: contents: write` scoped at job
  level, not workflow level.

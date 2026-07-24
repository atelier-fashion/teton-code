---
id: TASK-005
title: "Disk preflight + atomic install/verify pipeline"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
dependencies: [TASK-002]
---

## Description

BR-7 and BR-9: refuse before fetching when disk is short, and never present a
partial or unverified artifact as installed.

## Files to Create/Modify

- `crates/tetond/src/install.rs` — new: preflight (free space vs `size_bytes` + working margin), download to a temp path via the TASK-002 fetcher, SHA-256 verify, atomic rename into the weights dir, `InstallState` reporting (absent/partial/verified/corrupt)
- `crates/tetond/src/runtime.rs` — call the install pipeline after a decision; emit `model_lifecycle` progress throughout
- `crates/tetond/tests/install_pipeline.rs` — preflight, atomicity, corruption tests

## Acceptance Criteria

- [x] Insufficient free disk refuses **before any bytes are fetched**, naming required vs available (AC-6) — asserted via a fetcher double that must record zero calls
- [x] A mismatched digest discards the artifact, never installs it, and reports a clear error; `InstallState` never reports `verified` for a truncated file (AC-7)
- [x] Install is atomic: a crash/interrupt mid-download leaves no file at the final path (test by verifying only the temp path exists mid-flight)
- [x] Resume works across an interrupted download (the existing library contract, exercised end-to-end here)
- [x] Progress events are emitted for download start/progress/verify/install

## Technical Notes

The margin above `size_bytes` should account for the temp copy existing
alongside nothing else — document the chosen margin as a named constant rather
than a magic number. Reuse `teton_inference::Downloader` for the resume/verify
loop; this task is the surrounding install/preflight shell, not a reimplementation.

## Implementation Notes (as built)

- The margin is **not** redefined here: the pipeline consumes TASK-004's
  `model_consent::DISK_WORKING_MARGIN_BYTES` through `required_disk_bytes()`, so
  the figure the proposal advertises and the figure the preflight enforces are
  one value by construction. A resumed download subtracts the bytes already on
  disk from that requirement.
- `install_status` is no longer a size check. A successful install writes a
  verification receipt (digest + size + mtime) beside the weights; `status()`
  reports `verified` only when the receipt still describes the file, and
  otherwise re-digests rather than guessing. `WeightsInstall::deep_status()` is
  the always-re-digest read.
- The precise failure cause is recovered from TASK-002's
  `HttpRangeFetcher::last_error()` through a `FetchCause` seam, so a 429 is
  reported as `InstallError::RateLimited` rather than as a generic transport
  failure or as corruption (AC-12).
- **Deviation**: `verify` and `install` progress are reported on the
  `InstallProgress` seam but project onto the wire's existing `download` stage.
  `ModelLifecycleStage` has no `verifying`/`installing` variant and is matched
  exhaustively by the CLI, so adding one belongs with the client that renders
  it. Wire progress is therefore download start/progress/completion; the gate's
  existing `ready` remains the install-complete signal.

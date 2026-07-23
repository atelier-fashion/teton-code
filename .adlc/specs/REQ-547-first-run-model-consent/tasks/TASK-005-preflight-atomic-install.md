---
id: TASK-005
title: "Disk preflight + atomic install/verify pipeline"
status: draft
parent: REQ-547
created: 2026-07-21
updated: 2026-07-21
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

- [ ] Insufficient free disk refuses **before any bytes are fetched**, naming required vs available (AC-6) — asserted via a fetcher double that must record zero calls
- [ ] A mismatched digest discards the artifact, never installs it, and reports a clear error; `InstallState` never reports `verified` for a truncated file (AC-7)
- [ ] Install is atomic: a crash/interrupt mid-download leaves no file at the final path (test by verifying only the temp path exists mid-flight)
- [ ] Resume works across an interrupted download (the existing library contract, exercised end-to-end here)
- [ ] Progress events are emitted for download start/progress/verify/install

## Technical Notes

The margin above `size_bytes` should account for the temp copy existing
alongside nothing else — document the chosen margin as a named constant rather
than a magic number. Reuse `teton_inference::Downloader` for the resume/verify
loop; this task is the surrounding install/preflight shell, not a reimplementation.

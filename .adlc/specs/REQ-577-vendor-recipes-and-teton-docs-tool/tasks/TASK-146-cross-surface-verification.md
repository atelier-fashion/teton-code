---
id: TASK-146
title: "Cross-surface verification: egress, offline, gated sweep, changelog"
status: draft
parent: REQ-577
created: 2026-08-14
updated: 2026-08-14
dependencies: ["TASK-144", "TASK-145"]
repo: teton-code
---

## Description

Prove the cross-cutting properties: zero egress and empty provenance for
`teton_docs`, offline exposure, full-workspace freshness, the LESSON-515
gated-target sweep, and the release notes (spec BR-6, BR-7; AC-5, AC-6).

## Files to Create/Modify

- `crates/tetond/tests/offline_session.rs` — new test: an offline session
  serves a scripted `teton_docs` call (ScriptedEngine fixture), the reply
  carries the topic body, and zero egress occurs.
- `crates/tetond/tests/egress_capture.rs` (or the suite the capture
  transport actually lives in — confirm at implementation) — new test: a
  session issuing `teton_docs` calls records no egress requests; provenance
  for the outcome carries no paths (AC-6).
- `CHANGELOG.md` — `[Unreleased]` entry: vendor recipe catalog (six vendors,
  CI-gated prose), `teton_docs` tool (four topics, cap-exempt, offline,
  zero-egress), referral clause.

## Acceptance Criteria

- [ ] New offline + egress tests green and non-vacuous (each asserts the
  positive content of the reply, not just the absence of egress —
  LESSON-520's pairing rule).
- [ ] Full workspace build then test per the BUG-164 freshness rule:
  `cargo build --workspace` followed by `cargo test --workspace
  --no-fail-fast` — all green, count reported honestly.
- [ ] Gated sweep passes: `cargo check -p tetond -p teton-inference
  --features tetond/llama,teton-inference/llama --tests` (LESSON-515;
  `template_smoke` consumes `with_builtins()` and must still compile).
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo fmt --all --
  --check` clean; `tools/release/changelog-section.sh` accepts the new
  section (exit 0) if that gate exists on this branch.

## Technical Notes

- The capture-transport utilities live around egress_capture.rs:68
  (`CaptureTransport`) and offline_session.rs:37 (`ScriptedEngine`),
  offline_session.rs:117 (`temp_repo`) — reuse, don't reinvent.
- If a first run of the workspace suite reports failures, remember
  fail-fast hides targets: re-run with `--no-fail-fast` before diagnosing
  (repo memory).

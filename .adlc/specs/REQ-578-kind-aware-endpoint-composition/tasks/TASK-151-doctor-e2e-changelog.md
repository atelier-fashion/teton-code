---
id: TASK-151
title: "Doctor advisory, e2e acceptance, changelog, gates"
status: draft
parent: REQ-578
created: 2026-08-15
updated: 2026-08-15
dependencies: ["TASK-149", "TASK-150"]
repo: teton-code
---

## Description

Close the REQ: the doctor advisory (ADR-4), the AC-1..AC-5 e2e suite through
a real daemon, the changelog entry, and the full workspace gates.

## Files to Create/Modify

- `crates/teton/src/main.rs` — doctor pass over remote providers applying
  the classifier's would-compose predicate; one `LineKind::Notice` per
  class-(b) endpoint naming the exact full form; unit test for the advisory
  rendering (flagged vs custom-path-silent).
- `crates/teton/tests/cli_e2e.rs` — new e2e tests per ADR-5: AC-1
  (base-URL composes through real config/set and persists the full URL),
  AC-2 (idempotence, no echo), AC-3 (Anthropic default + ordering), AC-4
  (custom path verbatim), AC-5 (doctor advisory, exit status unchanged —
  extend the existing doctor e2e).
- `CHANGELOG.md` — `[Unreleased]` entry: base URLs now compose at
  registration, Anthropic endpoint defaults, the echo line, the doctor
  advisory; note hand-edited configs are untouched.

## Acceptance Criteria

- [ ] All five e2e ACs green against a spawned daemon (TestDaemon fixture);
  AC-5's advisory does not change doctor's exit status.
- [ ] Full gates: `cargo build --workspace` then `cargo test --workspace
  --no-fail-fast` (counts reported honestly), `cargo clippy --workspace
  --all-targets` clean, `cargo fmt --all -- --check` clean,
  `tools/release/changelog-section.sh` exit 0, and the LESSON-515 gated
  sweep (`cargo check -p tetond -p teton-inference --features
  tetond/llama,teton-inference/llama --tests`).
- [ ] AC-6 audit repeated at REQ level: zero diff on the three protected
  files across the whole branch.

## Technical Notes

- The doctor e2e at cli_e2e.rs:393
  (`teton_doctor_and_cost_report_against_a_live_daemon`) is the fixture to
  extend — inject a bare-`/v1` provider via hand-written config (the
  hand-edit path is exactly what the advisory exists for).
- REQ-576 presence attestation degrades to allow on no-presence builds, so
  the e2e needs no seams (integration explorer confirmed).

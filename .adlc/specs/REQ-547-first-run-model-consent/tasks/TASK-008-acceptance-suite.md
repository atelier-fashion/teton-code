---
id: TASK-008
title: "Acceptance suite AC-1..AC-12 + manual-gate runbook"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
dependencies: [TASK-005, TASK-006, TASK-007]
---

## Description

End-to-end verification against the real `tetond`/`teton` binaries, plus the
written runbook for AC-13 (the one claim CI cannot make).

## Files to Create/Modify

- `crates/tetond/tests/e2e/consent_matrix.rs` — one test per AC-1..AC-10, AC-12 against the spawned daemon with a mock HF server (serving a tiny fake "GGUF" with a real computed digest) and simulated hardware/disk profiles
- `crates/tetond/tests/e2e/harness.rs` — extend: mock HF endpoints (tree API + resolve + a 302 to a second host), disk-space override, `TETON_*` env for probe simulation
- `crates/teton/tests/cli_e2e.rs` — extend: real `teton` binary drives an interactive accept and a `--yes` run
- `docs/manual-verification.md` — new: the AC-13 runbook (build `--features llama`, install a real catalog model, run a session, record observed first-token latency and tok/s) with a sign-off line
- `.adlc/specs/REQ-547-first-run-model-consent/requirement.md` — check off verified ACs (modify, at completion)

## Acceptance Criteria

- [x] Each of AC-1..AC-10 and AC-12 has a distinct test that fails when its feature is broken; mutation spot-check **AC-1 and AC-7** specifically (break, observe red, revert — report it)
- [x] The suite runs in CI with no real model weights and no network to huggingface.co (mock server only) — TASK-006's integrity check is the only network-touching job
- [x] AC-13 is **not** auto-checked anywhere; the runbook exists and the spec's AC-13 stays unchecked until a human signs off
- [x] Full workspace green: `cargo test --workspace`, `clippy -D warnings`, `fmt --check`
- [x] Suite completes in under ~2 minutes

## Technical Notes

Mock the HF surface rather than hitting the network so the suite is hermetic and
fast; the *real* HF contract is what TASK-006 verifies. Serving a small fake
artifact with a genuinely computed SHA-256 keeps the verify path honest without
moving gigabytes. Do not mark AC-13 complete under any circumstance — that is
the point of a manual gate (LESSON-433).

---
id: TASK-006
title: "teton-inference: llama.cpp embed, probe, download, benchmark"
status: draft
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-003]
---

## Description

Local model tier per BR-8/BR-9 and AC-8: embedded llama.cpp inference,
hardware probe with decision table, first-run GGUF download with progress,
post-download micro-benchmark with auto-step-down, and runtime memory-pressure
adaptation.

## Files to Create/Modify

- `crates/teton-inference/src/engine.rs` — llama.cpp wrapper (llama-cpp-2 or vendored): load GGUF, stream completion, Metal on Apple Silicon
- `crates/teton-inference/src/probe.rs` — hardware probe (RAM, disk, GPU class) → candidate tier per OQ-3 table (<8GB none, 8–16GB 1.5B–3B, 16–32GB 7B, 32GB+ optional 30B-A3B)
- `crates/teton-inference/src/download.rs` — resumable GGUF fetch, checksum verify, progress events (`model_lifecycle`)
- `crates/teton-inference/src/benchmark.rs` — micro-benchmark (first-token latency, tok/s on classification + summary prompts); step-down when BR-8's ≤1s duty fails
- `crates/teton-inference/src/pressure.rs` — memory-pressure watcher; unload/downgrade instead of swap-thrash; reload on recovery (BR-9)
- `crates/teton-inference/tests/probe_table.rs` — table-driven probe tests over simulated hardware profiles

## Acceptance Criteria

- [ ] Probe decision table matches OQ-3 for simulated profiles incl. <8GB → local tier disabled, sessions proceed remote-only (AC-8)
- [ ] Forced-slow benchmark triggers step-down to next smaller model; step-down chain terminates at disabled, never loops (AC-8)
- [ ] Download resumes after interruption; checksum mismatch discards and re-fetches
- [ ] Simulated memory pressure unloads the model and emits `model_lifecycle`; inference requests during unload return typed "local tier unavailable" (router bypasses per BR-8)
- [ ] User-pinned model in config overrides the probe (BR-9)

## Technical Notes

Real-inference tests must be `#[ignore]`d behind a feature flag (CI has no
weights); everything else tests against a mock engine trait. Model catalog
(name → url, sha, size, RAM floor) is data, not code — a versioned TOML the
daemon can update independently of releases.

---
id: BUG-219
title: "A failed BR-8 latency duty disables the local tier instead of stepping down to the next smaller model"
status: resolved
severity: high
created: 2026-09-09
updated: 2026-09-09
resolved: 2026-09-09
component: "tetond/model_consent"
domain: "local-tier"
stack: ["rust"]
concerns: ["reliability", "developer-experience"]
tags: ["br-8", "benchmark", "step-down", "consent-gate", "req-544", "req-547"]
introduced_by: ["REQ-547"]
attribution: manual
---

## Description

REQ-544 promised that a model which misses the post-load micro-benchmark's
latency duty (BR-8: first token within 1000ms, decode at 5 tok/s or better)
steps down to the next smaller catalog model that fits the machine. The
library carries that walk (`teton_inference::benchmark::benchmark_with_step_down`)
and the protocol carries its `stepped_down` lifecycle stage, but nothing in the
daemon calls the walk. REQ-547's consent gate — the only path that loads and
benchmarks a real engine — answers a failed duty with `disabled` and
`EngineLoadFailed`, so the tier is gone for the daemon's lifetime, the failure
is not persisted, and the next daemon start pays the same multi-gigabyte load
and the same failing benchmark again.

## Reproduction Steps

1. A 48 GiB Apple Silicon machine; the probe proposes `qwen3-coder-30b-a3b`
   and the user accepts.
2. Start a session. The engine loads; the benchmark reports
   `first token 2021 ms, 27.4 tok/s`.
3. `local tier disabled: first-token latency 2021ms exceeds the 1000ms duty (BR-8)`.
   `teton doctor`, `teton policy show` and every turn that needs the local tier
   (a shell-pinned session, `route`, `title`, `digest`, `compact`, `triage`)
   report the tier unavailable. `qwen2.5-coder-7b` would have passed and was
   never tried.

Seen 2026-09-09 as the second layer of a three-layer failure: a `/analyze`
preamble pinned the session to the local tier (toolkit BUG-220), the local tier
had been disabled by this bug, and the resulting turn error suggested setting
`default_provider`, which was not the cause.

## Expected Behavior

The failed measurement is published, the tier announces `stepped_down` from the
model that missed to the next smaller one that fits, that model goes through the
one install path (downloaded if absent, verified, loaded, benchmarked), and the
tier reaches `ready` on the first model that passes — or `disabled` only when
nothing smaller fits.

## Actual Behavior

`disabled` on the first miss, with no step and no memory of it.

## Environment

- Teton Code 0.1.32, macOS, 48 GiB, `qwen3-coder-30b-a3b` selected by the probe.

## Root Cause

REQ-547 moved loading and benchmarking behind the consent gate
(`ModelConsentGate::activate_engine`) so a real engine could be staged and
committed under the user's decision. The gate's duty-failure arm was written to
the single-model shape the gate knew — abandon the engine, publish `disabled`,
return `EngineLoadFailed` — and the REQ-544 walk, which predates the gate and
takes a `measure` closure rather than an install path, was never connected to
it. The `stepped_down` stage was only ever produced by the `TETON_FORCE_BENCH`
simulation in `runtime/engine.rs`.

## Resolution

`activate_engine`'s duty-failure arm now asks `step_down_target`. For a
selection the daemon made — `probe`, `auto_accept`, or an earlier `step_down` —
it publishes `stepped_down { from, to, reason }` and re-enters `commit` for the
smaller catalog entry (boxed: `commit → run_install → report_install_success →
activate_engine` is the recursion), so the smaller model is recorded, installed
if it is not already on disk, verified, loaded and benchmarked in its turn, and
can step down again. A `user_override` (`teton model set`) is the user's choice
and is not revised: the tier is disabled with the measurement plus the command
that picks a smaller model (REQ-544 BR-9, "a user pin always overrides", read in
both directions). An exhausted chain is disabled with a reason that says so.

The step is recorded with a new `SelectionSource::StepDown` (wire `step_down`,
CLI label "stepped down after a failed benchmark"), so the next daemon start
loads the model that passed instead of paying the failed load again;
`teton model set <name>` remains the way back up.

Tests: `a_probe_pick_that_misses_the_duty_steps_down_to_the_next_model_that_passes`
and `a_user_override_that_misses_the_duty_is_disabled_not_stepped_down` in
`crates/tetond/tests/model_consent.rs`; the existing duty-fail test keeps its
assertions (its `small-fit` is a user override).

## Deployment

- Merged as 894e05f (PR #312), 2026-09-09. Ships in v0.1.33. LESSON-656.

## Files Changed

- `crates/tetond/src/model_consent.rs` — the step-down arm, `step_down_target`, `StepDown`
- `crates/teton-core/src/entities.rs`, `crates/teton-protocol/src/events.rs` — `SelectionSource::StepDown`
- `crates/teton/src/firstrun.rs`, `crates/teton/src/model_ui.rs` — the label
- `crates/tetond/tests/model_consent.rs` — `build_with_catalog`, the two tests
- `crates/tetond/src/harness/tools/shell_provenance.rs` — `git worktree` and `git for-each-ref` as name-only verbs (for toolkit BUG-220; same change set)

---
id: TASK-039
title: "LoadingIndicator: a pure state machine whose frames need no terminal"
status: complete
parent: REQ-556
created: 2026-08-04
updated: 2026-08-04
dependencies: []
---

## Description

The indicator's behaviour as a pure function of observed lifecycle stages and a
tick counter (ADR-556-2, BR-11). No I/O, no terminal, no clock — so the whole of
REQ-556's core behaviour is assertable from a plain unit test even though BR-2
makes it invisible to every piped e2e.

This task ships no user-visible change on its own; TASK-040 wires it in. It is
separated precisely so the logic is testable without the terminal plumbing.

## Files to Create/Modify

- `crates/teton/src/loading.rs` — **new**: `LoadingIndicator`, `observe(&mut self, &ModelLifecycleStage)`, `frame(&self, tick: u64) -> Option<String>`
- `crates/teton/src/main.rs` — `mod loading;` declaration only

## Acceptance Criteria

- [ ] `observe` moves the indicator to a working state on `Download` and
      `Verifying`, and out of it on `Ready`, `Benchmark`, `SteppedDown`,
      `Disabled` and `AwaitingDecision` (BR-6 — an indicator implies work is
      happening; a tier waiting on the user is not working).
- [ ] `frame` returns `None` whenever nothing should be drawn, so "hidden" is
      representable and is the default.
- [ ] **AC-3 (a)**: `frame` returns a fraction **only** when the observed stage
      carried byte counts; with `total_bytes: None` it returns motion and no
      fraction (BR-5).
- [ ] **AC-3 (b)**: `frame` contains no elapsed-time or ETA arithmetic. It has
      no clock input other than `tick`, and `tick` must not be converted to a
      remaining-time estimate anywhere (BR-5). Assert the rendered string never
      matches an ETA-shaped pattern for any tick in a long sweep.
- [ ] **AC-3 (c)**: `firstrun::render_lifecycle`'s and `progress_bar`'s existing
      unit tests pass **unmodified** — this task reuses them and adds no second
      rendering of any stage (BR-10). A test edited to accommodate this module
      is a BR-10 violation, not an accommodation.
- [ ] **AC-6**: bounded termination — past a tick cap the indicator stops
      advancing and `frame` yields one static line naming the last stage
      actually observed (BR-7, ADR-556-3). Test drives a sequence that never
      reaches `Ready`.
- [ ] The frame sequence advances with `tick` — two different ticks in the
      working state produce different strings (this is what TASK-042's mutation
      check breaks).
- [ ] Every criterion above is covered by a unit test that constructs no
      `Surface` and opens no terminal.

## Technical Notes

- `ModelLifecycleStage` is in `teton-protocol` (`crates/teton-protocol/src/events.rs:330`):
  `Probed`, `AwaitingDecision`, `Download { downloaded_bytes, total_bytes: Option<u64> }`,
  `Verifying { total_bytes }`, `Benchmark { first_token_ms, tokens_per_sec }`,
  `Ready`, `SteppedDown`, `Disabled`.
- There is **no** `Loading` stage. The window this REQ exists for is the silence
  *after* `Verifying` and *before* `Benchmark` — the state machine must treat
  "last observed `Verifying`, nothing since" as the animated state.
- BR-10: do not render the per-stage lines here. `firstrun::render_lifecycle`
  (`crates/teton/src/firstrun.rs:31`) already renders all eight stages and keeps
  doing so. This module renders **only** the motion for the silent window.
- Reuse `firstrun::progress_bar` (`firstrun.rs:82`) if a determinate fraction is
  drawn at all — do not write a second bar (BR-10).
- Keep the model id out of any filesystem-path shape (REQ-547 BR-11); it arrives
  as a plain id from the daemon.

---
id: TASK-295
title: "Migrate the duty-routing family to DutyContext and clear the vestigial suppressions"
status: draft
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-294]
---

## Description

Thread `DutyContext` through the eight duty-routing functions, and remove the
suppressions that stop being needed — including the seven that never suppressed
anything.

The four calls in `run_one_attempt` to `digest_route`, `triage_route`,
`shell_route`, and `compact_route` currently pass the **identical six
arguments** in the identical order. That repetition is the evidence this bundle
is real; after this task each call is `self.digest_route(dctx)`.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `digest_route`, `triage_route`, `shell_route`,
  `title_route`, `compact_route`, `resolve_duty`, `build_duty_route`,
  `spawn_title_session`, and their call sites

## Acceptance Criteria

- [ ] All five `*_route` functions take `(&self, dctx: DutyContext<'_>)`.
- [ ] `resolve_duty` and `build_duty_route` take the duty, the route, and a
      `DutyContext`.
- [ ] `spawn_title_session` builds its `DutyContext` directly (it has no gate).
- [ ] AC-2: neither `resolve_duty` nor `build_duty_route` retains a stacked pair
      of `#[allow(clippy::too_many_arguments)]`, and each ends with **zero**.
- [ ] The five `*_route` functions each end with zero suppressions (they were
      vestigial — exactly 7 args, at clippy's threshold).
- [ ] BR-1: no event payload, ordering, or dispatch decision changes. TASK-293's
      fixture test still passes.
- [ ] BR-8: every REQ/ADR/LESSON/BUG id in a moved doc comment moves with the
      code it annotates. The four call-site comments in `run_one_attempt`
      (REQ-558 TASK-054, REQ-561 TASK-060/061/063) still sit with their calls.
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test --workspace
      --no-fail-fast` green with output grepped for `FAILED`.

## Technical Notes

`spawn_title_session` is a **detached** task that outlives its triggering prompt
(its doc comment records why it gets no spend accumulator — REQ-588). Its
`DutyContext` must therefore not borrow anything tied to the turn's lifetime in
a way that forces a lifetime change on the spawn. If the borrow does not fit,
build the context inside the spawned body from owned clones rather than widening
the struct — and say so in a comment.

`build_duty_route`'s doc comment explains that the category name is read off the
duty "so two surfaces describing one routing state must not be able to drift."
That comment is about the code immediately under it; keep them together.

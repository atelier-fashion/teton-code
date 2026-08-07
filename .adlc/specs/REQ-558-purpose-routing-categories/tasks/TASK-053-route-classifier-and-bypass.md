---
id: TASK-053
title: "The route classifier: a local model call, with a bypass that costs nothing"
status: complete
parent: REQ-558
created: 2026-08-05
updated: 2026-08-06
dependencies: [TASK-050, TASK-051]
---

## Description

Build the `route` category — the classifier that assigns a freeform prompt to one
of the four judgment categories. It does not exist today; the thing it replaces is
a ten-word substring list.

**This is a new model call on a path that had none.** It runs only for freeform
judgment turns (ADR-C): a structured turn derives its category from its phase with
no model call.

## Files to Create/Modify

- `crates/tetond/src/classify.rs` — **new**: the classifier, returning
  `JudgmentCategory`
- `crates/tetond/src/router.rs` — freeform judgment turns consult it
- `crates/tetond/src/runtime.rs` — wire the local engine in; bypass when absent

## Acceptance Criteria

- [x] **AC-1, the direct regression**: in a freeform session with `think` bound to
      a frontier provider, *"explain the tradeoffs between these two
      architectures"* routes to the `design`/`think` binding, **not** to the local
      tier. This test fails against today's binary.
- [x] The classifier returns `JudgmentCategory` — four variants. It is
      type-impossible for it to return `digest` or any other harness-known
      category (AC-3, BR-2).
- [x] With the local tier unavailable, classification is **bypassed**: the turn
      resolves through the BR-9 declared default, `route_decided` names the
      bypass, and **no remote classification call is issued** — asserted by call
      count, not by output text (AC-5, BR-5).
- [x] A classifier failure (engine error, unparseable output) falls back to the
      declared default and says so — never to silence, never to a remote call
      (BR-3, BR-9).
- [x] A structured turn issues **zero** classifier calls (ADR-C) — asserted by
      call count.
- [x] Every classification emits `route_decided` naming the category, tier,
      provider, and the signal that fired (BR-3).

## Technical Notes

**Bypass is not "route the classifier remotely".** BR-5 is explicit and REQ-544
BR-8 is the reason: the local tier's value is latency, and a remote call to decide
where to send a call costs more than the decision saves. The bypass path must issue
no network request at all — hence the call-count assertion rather than a text check.

**The fallback must preserve the guarded property** (LESSON-447). The classifier's
job is to pick a category; when it fails, the degraded means is the declared
default — a real category, resolved through the same chain — and the degradation
must be *visible* in `route_decided`. An `Err(_) => silently pick something` is the
shape that lesson forbids.

**Latency is the risk, and it is new.** Every freeform judgment turn now waits on a
local classification before the real call starts. Keep the classifier prompt and
its output budget small, and prefer a constrained output (a single token/word) over
free text.

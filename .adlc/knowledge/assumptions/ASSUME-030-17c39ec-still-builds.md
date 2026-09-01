---
id: ASSUME-030
title: "`17c39ec` still builds, so the missing scenarios can be captured pre-split"
status: validated
req: REQ-604
created: 2026-09-01
resolved: 2026-09-01
---

## Assumption

REQ-604's spec carried exactly one assumption, and made its failure branch
explicit rather than optimistic:

> `17c39ec` still builds. If it does not, this REQ becomes "record that the
> scenarios cannot be captured and say what covers them instead" — which is a
> real outcome, not a failure to be papered over.

The whole REQ rests on it. The two missing sequences could not be recorded at
tip without producing a golden file computed by the subject it checks
(LESSON-569), so if the capture commit no longer built, there was no fixture to
have — only a finding.

## Disposition: **validated**

Checked at the start of Phase 2, before any design work was committed to:

- `cargo build --tests -p tetond` at `17c39ec` — clean.
- A sample existing test run there — green, against a suite of 1,932 lib tests.
- The pre-split single-file `crates/tetond/src/runtime.rs` (36,434 lines) is
  present, with the `carry_runtime`, `prompt`, `one_session_with`,
  `await_permission_request` and `Scripted` helpers the harness needed.

The fallback outcome was therefore not taken.

## What It Cost To Check, And Why It Was Worth Checking First

Verifying it took one build. Had it been assumed and been false, it would have
been discovered after the fixtures were designed, the replay tests written, and
the provenance headers drafted — with nothing to record in them.

Two facts found alongside it shaped the design more than the assumption itself:

- `run_prompt_turn`'s ten-argument signature is **identical** at `17c39ec` and
  at tip, so the harness and the replay could drive the subject with the same
  code (see LESSON-609 for why that still had to be diffed rather than assumed).
- The `req598_event_order` test module does **not** exist at `17c39ec` — only
  the fixture was captured there, on REQ-598's branch base. So a harness had to
  be built rather than reused, which is why this assumption was load-bearing at
  all.

## Residual

None. The assumption is closed. Note that it is inherently non-recurring: it was
a claim about one historical commit, and a future REQ needing a pre-split capture
must re-verify its own capture commit rather than inherit this result.

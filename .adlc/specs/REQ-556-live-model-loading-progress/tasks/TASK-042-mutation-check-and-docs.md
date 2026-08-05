---
id: TASK-042
title: "Mutation-check the feature and document the loading window"
status: complete
parent: REQ-556
created: 2026-08-04
updated: 2026-08-04
dependencies: [TASK-040, TASK-041]
---

## Description

Prove the suite actually fails when the feature is disabled (AC-8), and record
the user-visible behaviour where the project records such things. A suite that
stays green with the animation frozen has not tested the animation — that is the
whole of LESSON-441 and LESSON-464.

## Files to Create/Modify

- `crates/teton/src/loading.rs` — mutation-check documentation comment naming what breaks which test
- `crates/teton/tests/pty_e2e.rs` — the idle-render mutation assertion, if it belongs at e2e level rather than unit
- `docs/manual-verification.md` — the restart step gains the indicator observation
- `README.md` — one line in the session section, if the behaviour is user-facing enough to warrant it

## Acceptance Criteria

- [ ] Freezing the tick (making `frame` ignore its `tick` argument) fails at
      least one test. Demonstrate by actually applying the mutation and
      recording the failing test name — not by asserting it would.
- [ ] Removing the idle-render path (reverting the loop to block on
      `read_line`) fails at least one test — the AC-2 pty leg is the expected
      one; confirm it is.
- [ ] `docs/manual-verification.md` §6's restart step tells a human what they
      should now see during the load window, replacing the BUG-152 notice line
      added earlier with the live behaviour.
- [ ] Every AC in the requirement is traceable to a test or a manual-verification
      line — produce the mapping and record any AC that ends up with neither
      (there should be none; if there is, that is a finding, not a footnote).

## AC traceability (produced 2026-08-04)

| AC | Route | Where |
|---|---|---|
| AC-1 | unit | `main::the_indicator_paints_through_the_surface_and_reports_its_row_count` |
| AC-1 | **pty — NOT COVERED** | recorded in TASK-041; manual-verification §6 is the coverage |
| AC-2 | pty | `pty_e2e::an_idle_session_renders_an_event_with_nothing_typed` (mutation-verified) |
| AC-3 (a) | unit | `loading::a_fraction_appears_only_when_the_daemon_supplied_a_total` |
| AC-3 (b) | unit | `loading::no_frame_ever_renders_an_eta` (500-tick sweep × 4 phases) |
| AC-3 (c) | existing | `firstrun::render_lifecycle_covers_every_stage`, `firstrun::progress_bar_tracks_percent_and_clamps` — both unmodified |
| AC-4 | piped | `cli_e2e::slash_quit_ends_the_session_exactly_as_ctrl_d_does`, `an_escaped_line_and_a_plain_line_both_reach_the_model` — both unmodified |
| AC-5 | pty | **partial** — the pty session accepts input and the frame survives, but no line is typed *while the indicator animates* (needs the load window, as AC-1's pty leg does) |
| AC-6 | unit | `loading::an_indicator_that_never_hears_ready_stops_and_says_what_it_saw` |
| AC-7 | CI | both `fmt · clippy · test` legs (macos-latest, ubuntu-latest) |
| AC-8 | mutation | both mutations applied and observed failing; recorded in `loading.rs`'s module docs |

**Two ACs are not fully closed** — AC-1's pty leg and AC-5's
"while-animating" half — and both fail for the *same* reason: no test seam can
hold the tier in its load window from this crate. Neither is quietly dropped;
both are on the manual-verification checklist and the way to close them (lift
`MockHf` into shared test support) is recorded in TASK-041.

## Technical Notes

- The two mutations are the ones that matter because they correspond to the two
  halves of the REQ: the state machine's motion (TASK-039) and the loop's
  timeliness (TASK-038). A mutation that only breaks a compile is not evidence
  (LESSON-464: a new control needs its own known-bad in the same pass).
- `docs/manual-verification.md` already gained a BUG-152 line in §6 about the
  `>> model still loading —` notice. That notice is the *fallback* now (BR-9),
  not the primary experience — update it rather than adding a second bullet that
  contradicts it.
- Keep the README edit small or skip it with a note; the in-session command
  table there is about commands, and this is ambient behaviour.

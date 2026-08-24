---
id: TASK-251
title: "Client: render and answer the over-budget offer"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: [TASK-241, TASK-243]
---

## Description

BR-4 + ADR-2. `PermissionSubject` is matched exhaustively client-side, so the new variant forces every arm. The no-terminal and unrecognized paths must refuse, never proceed.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — `consent_gate` (2836), `resolve_permission` (2891), `render_consent_subject` (~3042), summary/echo renderers (~3153)

## Acceptance Criteria

- [x] `consent_gate`'s `RefuseNoTerminal` arm fires BEFORE `prompter.ask` reads a line, so a piped answer for a later prompt cannot be swallowed as this one
- [x] The rendered offer quotes the same figures the measurement produced and names the bound verbatim (AC-2)
- [x] The four options render as a single-select; the remedy options are absent when the bound has no remedy
- [x] A project-sourced skill's name renders under the distinguishing treatment project skills already get, never as bare harness vocabulary (ASSUME-018)
- [x] Reuse `bound_clause`/`bound_words` (1944, 1975) and `budget_clause` (3282) — no second vocabulary for the same fact (LESSON-456)
- [x] A client-side test asserts the no-terminal path returns `Refused { reason: NoTerminal }` and never `Cancelled`, and that it fires before any read of stdin (BR-4)
- [x] A rendering test pins the offer's rendered text for each of the five bounds, driven from a constructed `PermissionRequest` — and a producer-side test proves the daemon actually emits that subject (LESSON-544: a struct-literal test alone leaves the producer unguarded)
- [x] Removing the `SkillOverBudget` arm fails compilation, demonstrating the exhaustive-match forcing function (ADR-2)

## Technical Notes

An older client hits the `#[serde(other)]` Unrecognized arm and refuses rather than mis-rendering — BR-4-compatible; leave it intact.

## Implementation notes

**The three forced arms are closed and `cargo build --workspace` is green.**
`render_event` (three event arms), `consent_gate` (one arm) and
`render_consent_subject` (one arm) — ADR-2's forcing function working exactly as
designed, and the compile stayed red from TASK-241 until this landed.

**`resolve_permission` does not redden, and it held two live defects.** The
compiler points at neither, per ADR-2's Correction. Both were found by reasoning
about the function rather than by following an error:

1. **BR-4's ordering held, and is now pinned.** `consent_gate` was already
   first, so the new `RefuseNoTerminal` arm fires before `prompter.ask`. What was
   missing was a test:
   `a_piped_over_budget_offer_is_refused_without_reading_a_line` scripts the `y` a
   paste would have queued and asserts `prompter.asked == 0`, the outcome
   `Refused { NoTerminal }`, and explicitly **not** `Cancelled`. Mutation-checked:
   answering the subject `Answerable` on a pipe reddens it.

2. **A remembered grant would have answered the offer** (BR-10, ADR-14). The
   offer is asked under the *same* `skill:<source>:<name>` key REQ-585's
   dynamic-context consent is remembered under, so an `allow_always` from an
   earlier "run these four commands?" sat right in `SessionGrants`. The standard
   path would have read it and let `allow_outcome` pick by
   `PermissionOptionKind` — which cannot tell the four over-budget ids apart —
   auto-answering "send it whole", or `over_budget_proceed_and_remedy`, which
   also writes config. The deny direction was worse: with no `RejectAlways` on
   the offer, `deny_outcome` falls back to the first `RejectOnce`, which is
   `over_budget_remedy_only` — a **config write from a grant that said deny**.
   `resolve_over_budget_offer` now branches above both lookups and takes no
   `&mut SessionGrants` at all, which is `interpret_over_budget`'s trick: the
   store is not in scope, so reading or writing one is a compile error rather
   than a discipline. Pinned by
   `a_remembered_grant_never_answers_an_over_budget_offer`, mutation-checked by
   moving the branch below the grants.

**A third defect, in `refusal_line`.** Its `NoTerminal` sentence names
`/permissions full` as the remedy. That is **false** for this subject —
`authorize_skill_over_budget` asks under `LevelAllow::DoesNotSettle`, so a
`full` session raises the question and lands back on the same refusal (ADR-14).
The over-budget arm gets its own sentence, which says so outright and names the
terminal instead. It deliberately points at no durable fix, because one bound
has none (BR-7b) and this line cannot see which bound it is looking at.

**ADR-16 landed mid-task, and the exhaustive destructure is what caught it.**
The arm binds every field with no `..`, so TASK-247 adding `sentence` was an
`E0027` before any test ran. The sentence renders **verbatim** and this side
composes nothing: the arm quotes none of the figures beside it, because the
sentence's head already carries the stage clause, both pairs and the spoken
bound, and a second rendering would be two spellings of one number — one of
which says "about". Pinned negatively by
`the_offer_renders_the_daemons_sentence_verbatim_and_re_states_nothing`, which
reddens if any figure is re-stated outside the daemon's sentence.

**The prompt reads numbers only, and that is a safety property.** The four ids
share `PermissionOptionKind` values by construction (ADR-1), so the offer is a
numbered single-select over the daemon's own list, in the daemon's own order
(BR-3's "leads with the remedy" *is* the order), returning that row's id
verbatim. No letter is a choice: a stray `y` re-asks. `y` is the single most
likely thing to be sitting in a paste buffer, and the cost of it meaning
something here is an oversized send nobody chose. Empty and EOF answer
`Cancelled`, which the daemon reads as a decline that writes nothing — the
pre-REQ-589 outcome.

**One vocabulary for one fact.** `budget_clause` was split into `figure_pair`
and `budget_figures`, which the route line, the offer's records and the accepted
record all read, so `(bound: local engine)` at the prompt is the same three
words as on the `/verbose` route line
(`the_offered_record_and_the_route_line_spell_one_budget`). The route line's
bytes are unchanged.

**ADR-13's two hedges are separate sentences and are pinned as such.**
`WindowVerdict::Unknown` renders a hedge naming *this build*, never
`WindowUnknown`'s "declares no window"; `RemedyKind::Unknown` is never rendered
as `NotOffered`. Both fixtures are produced by serde from a value this build has
never heard of, never constructed by hand.

**Gating.** The `offered` event is verbose-gated — the addressed prompt two
lines later draws every figure it carries — while `accepted` and
`remedy_applied` are not: one is the counterpart of a decline's unconditional
refusal line, and the other says a file on disk changed.

## Verification

- `cargo build --workspace` — **green** (this task closed it).
- `cargo test -p teton --bin teton --no-fail-fast` — 643 passed, 0 failed
  (23 new tests plus two new rows on `consent_gate`'s truth table).
- `cargo test -p teton --test pty_e2e` — 9 passed, 0 failed.
- `cargo clippy -p teton --all-targets` — clean; `rustfmt --check` on
  `session_ui.rs` — clean (hand-applied, no package-wide `cargo fmt`).
- `cli_rows::guide_tests` re-run for real now that the crate compiles — **passes**.
  TASK-258 could only argue this statically; it is confirmed.
- Four mutations killed: gate always-`Answerable`; branch below the grants;
  a re-stated figure line; a dropped sentence line.

## Carried out of this task

- **`crates/teton/tests/cli_e2e.rs::a_typed_invocation_names_the_swap_and_its_flags_and_counts_no_turn_budget`
  fails, and it is TASK-248's blast radius, not this task's.** ADR-10's new
  typed-path trust gate uses `LevelAllow::DoesNotSettle` for a **shadowing**
  project skill, so a piped session cannot acknowledge it at *any* level —
  `/permissions full` was tried and does not clear it. TASK-248 taught
  `skill_turn.rs` and `provenance_egress.rs` about the new question but could not
  see this fixture, because the `teton` crate was red and its e2e targets never
  compiled. The behaviour change deserves Phase 5's attention on its own terms: a
  piped or unattended session can no longer run a typed project skill that
  shadows a user skill. Not fixed here — `cli_e2e.rs` is outside this task's file
  ownership.
- **The producer-side end-to-end leg is TASK-253's and TASK-255's.** `teton` has
  no dependency on `tetond` and cannot drive the code that mints the subject, so
  what this task could pin is the *contract*: every test here builds the
  `permission_request` frame from the daemon's own JSON key spellings and
  deserializes it through the real serde path, so a renamed producer field stops
  deserializing. The "a real turn emits this" half needs both binaries.
- `tetond --lib`'s `runtime::tests::the_over_budget_offer::*` failures are
  TASK-247's uncommitted in-flight work; the failing set changed between
  consecutive runs, which is the architecture's documented mid-phase churn
  signature.

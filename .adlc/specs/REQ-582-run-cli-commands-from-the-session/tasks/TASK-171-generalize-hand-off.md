---
id: TASK-171
title: "Generic hand-off line: `in this session: /<a>, /<b>` from the mirror table, third in precedence"
status: complete
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: [TASK-169]
repo: teton-code
---

## Description

ADR-6 / BR-8: after the REQ-579 and REQ-581 arms of
`session_ui::hand_off_after_turn` return without printing, scan the
backtick-stripped reply for every mirrored row's `Mirror.shell`
(`contains_word`, case-sensitive), drop the ones whose `/<row>` the reply
also names, and print at most one Notice `in this session: /provider list,
/policy show` in table order. Expose the candidate list from `slash`
(`pub(crate) fn mirrored_rows() -> impl Iterator<Item=(&'static str /*name*/,
&'static str /*shell*/)>`) so the hand-off cannot drift from the table.

## Files to Create/Modify

- `crates/teton/src/session_ui.rs` — generic arm + `GENERIC_HAND_OFF_PREFIX`; doc the precedence (setup → connection → generic) and why (BR-8).
- `crates/teton/src/slash.rs` — `mirrored_rows()` accessor.
- `crates/teton/src/session_ui.rs` tests (~4460+ `hand_off_turn` helper) — AC-9's four cases: (1) reply names `teton provider list` and `teton policy show` → exactly one line `in this session: /provider list, /policy show`; (2) reply names `/provider list` (and no shell form) → nothing; (3) reply names `teton provider add …` → the REQ-579 line, not the generic; (4) reply "the teton binary is slow" → nothing; plus: reply names `teton provider list` **and** `/provider list` → nothing (dormancy per command); reply names `Teton Provider List` (capitalised) → nothing; piped (`tty=false`) → nothing.

## Acceptance Criteria

- [x] All AC-9 cases as unit tests; the existing REQ-579/REQ-581 hand-off tests unchanged and green.
      — `a_reply_that_recites_shell_twins_names_their_session_spellings` (case 1,
      plus the reversed mention and a single-row line),
      `a_reply_that_already_names_the_session_spelling_earns_nothing` (case 2,
      plus dormancy per command),
      `the_setup_hand_off_wins_over_the_generic_line` (case 3),
      `a_reply_that_names_no_mirrored_command_earns_nothing` (case 4, plus a
      non-mirrored row and the `tetond` boundary),
      `a_capitalised_mention_of_a_command_is_not_one`,
      `the_connection_hand_off_wins_over_the_generic_line`. No REQ-579/REQ-581
      test needed a change.
- [x] At most one line per turn under every combination (the consuming `take` guarantee holds; add a test that a second `hand_off_after_turn` prints nothing).
      — `the_generic_line_is_tty_only_and_prints_once_per_turn`: pipe, the
      consumed second call, and the following quiet turn. The connection arm
      gained the `return` the third arm needs.
- [x] The candidate list is read from the slash table (a test asserts adding a fake mirrored row to a fixture table changes the candidates — or, simpler, that the candidates equal the table's `mirror` rows).
      — `every_mirrored_row_is_a_candidate_of_the_generic_line` drives
      `slash::mirrored_rows()` row by row (the two setup recipes expect the
      REQ-579 line) and then all eight remaining twins at once, in table order;
      `the_generic_line_names_only_spellings_the_session_dispatches` pins the
      other direction. `slash`'s own `every_mirror_names_teton_plus_its_own_row`
      (TASK-169) already pins `mirrored_rows()` against `COMMANDS`, so no
      `cfg(test)` accessor was needed.

## Technical Notes

- Reply-side matching is case-sensitive by REQ-581's rule (a command is lowercase); prompt-side rules do not apply — this arm reads only the reply.
- `contains_word(haystack, needle)` (session_ui.rs ~1717) already handles multi-word needles.

## Implementation Notes

- The connection arm now `return`s after printing; without it the third arm
  would have run on a turn that had already spoken. That is the only change to
  either existing arm — no predicate, constant or test of theirs moved.
- Table order is `model list, model status, provider list, provider add,
  boundary list, boundary add, policy show, policy set-tier, policy
  set-category, doctor`, so AC-9's pair reads `/provider list, /policy show`.
- `provider add` and `policy set-tier` are mirrored rows **and**
  `PROVIDER_CLI_RECIPES` entries, so they can never reach the generic arm — the
  precedence decides for the sentence that carries a reason (BR-8). The
  table-driven test states this as a property rather than working around it.
- The generic line is `GENERIC_HAND_OFF_PREFIX` + the `/` spellings joined with
  `", "` — built rather than declared, since its subject is whatever the reply
  recited.
- No `cfg(test)` accessor was added to `slash`: `mirrored_rows()` is
  `pub(crate)`, and the table-vs-accessor equality is already pinned there. The
  only `slash` change is dropping the `#[allow(dead_code)]` TASK-169 left for
  this commit.

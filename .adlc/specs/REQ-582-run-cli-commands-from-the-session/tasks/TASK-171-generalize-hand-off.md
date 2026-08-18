---
id: TASK-171
title: "Generic hand-off line: `in this session: /<a>, /<b>` from the mirror table, third in precedence"
status: draft
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

- [ ] All AC-9 cases as unit tests; the existing REQ-579/REQ-581 hand-off tests unchanged and green.
- [ ] At most one line per turn under every combination (the consuming `take` guarantee holds; add a test that a second `hand_off_after_turn` prints nothing).
- [ ] The candidate list is read from the slash table (a test asserts adding a fake mirrored row to a fixture table changes the candidates — or, simpler, that the candidates equal the table's `mirror` rows).

## Technical Notes

- Reply-side matching is case-sensitive by REQ-581's rule (a command is lowercase); prompt-side rules do not apply — this arm reads only the reply.
- `contains_word(haystack, needle)` (session_ui.rs ~1717) already handles multi-word needles.

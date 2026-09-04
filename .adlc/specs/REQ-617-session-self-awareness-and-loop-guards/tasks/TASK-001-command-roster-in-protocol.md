---
id: TASK-001
title: "The session command roster as a derived table in teton-protocol, with a both-directions drift guard in the CLI"
status: draft
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

Give the daemon the fact it does not have: the names of the session's built-in
commands. Per ADR-1 the CLI's `COMMANDS` table cannot move (its rows carry
function pointers into CLI types), so what moves is a derived roster of three
strings per row, in `teton-protocol`, which both binaries already depend on.

## Files to Create/Modify

- `crates/teton-protocol/src/commands.rs` — new. `SessionCommand { name, effect,
  user_only }` and `SESSION_COMMANDS: &[SessionCommand]`, one row per command in
  `slash::COMMANDS` (29 today), in `/help` order. `effect` is a short clause, no
  trailing period, sized for the `teton_docs commands` page rather than the
  prompt. `user_only` is `true` for every row today; the field exists because the
  roster's whole claim is *the user runs these*, and a claim carried by a field
  can be asserted where a claim carried by a comment cannot.
- `crates/teton-protocol/src/lib.rs` — declare and re-export the module.
- `crates/teton/src/slash.rs` — test block only. The drift guard.

## Acceptance Criteria

- [ ] `SESSION_COMMANDS` has one row per `slash::COMMANDS` row, same order.
- [ ] The drift guard asserts **set equality** in both directions and its failure
      message names both files and the offending names on each side.
- [ ] Mutation: deleting one `SESSION_COMMANDS` row goes red; adding a spurious
      one goes red. Both are recorded in the test's doc comment.
- [ ] No `teton-protocol` row carries a function pointer, a `Mirror`, or any
      type the daemon cannot name.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton/src/slash.rs::tests::the_protocol_roster_and_the_command_table_are_the_same_set` | yes |
| AC-2 | test-case | `crates/teton/src/slash.rs::tests::the_protocol_roster_and_the_command_table_are_the_same_set` | yes |

The benign path is the unmodified tree: the guard must be silent on the 29 rows
as they stand, or it is asserting nothing about drift.

## Technical Notes

`aliases` are deliberately **not** carried. An alias is a way to *type* a
command; the roster's consumers name a command for the user to type, and naming
two spellings of one thing to a small model is how it invents a third. `/help`
already collapses aliases to the canonical name for the same reason (BR-7 of
REQ-582).

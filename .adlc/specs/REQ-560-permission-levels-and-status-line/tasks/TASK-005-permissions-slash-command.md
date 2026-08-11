---
id: TASK-005
title: "/permissions as a row in the existing COMMANDS table"
status: complete
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-004]
---

## Description

Add `/permissions` to the one dispatch table `/help` renders from (BR-14), with
bare `/permissions` reading the current level and `/permissions <level>` setting
it. Both work on a pipe — that is BR-10, the non-visual read path that keeps the
setting usable for exactly the users BR-9 hides the status row from.

**Scope fence:** this task adds `/permissions` and nothing else. `/effort` is
REQ-559 BR-9's row and is running concurrently — do not add, alias, or duplicate
it.

## Files to Create/Modify

- `crates/teton/src/slash.rs`:
  - one `CommandSpec` row: `name: "permissions"`, `aliases: &[]`,
    `args: Args::None`, placed beside `/verbose` and `/clear` (the commands
    about *this session* rather than about the machine)
  - `handle_permissions` — issues `session/permissions` on the session's own
    connection and renders through the `Surface` seam:
    - no argument → one line naming the current level
    - a valid level → set, then one line confirming it (or reporting it was
      already that level, from `changed: false`)
    - an unrecognised level → one line listing the four valid spellings with
      each level's `summary()`, and **no RPC issued**
  - `Args::None` with argument-bearing input handled inside the handler:
    `/permissions` legitimately takes an optional argument, so the row cannot be
    `Args::Required`. Confirm whether the existing `Args` enum needs an
    `Optional(&'static str)` variant and add it if so, updating the resolve-time
    rejection logic and its tests

## Acceptance Criteria

- [ ] **AC-9 (piped)**: bare `/permissions` prints the current level on a pipe
- [ ] **AC-11**: `/help` lists `/permissions` from `COMMANDS`, and REQ-555's
      bidirectional table test (every row reachable from parsed input, every
      parsed command reaching a row) still passes with the new row — the
      `COMMANDS.len()`-derived assertions in `slash.rs` update to match
- [ ] `/permissions <valid>` sets the level and the next `/permissions` reads
      the new value back
- [ ] `/permissions bogus` renders one line listing the four levels, issues no
      RPC, and leaves the level unchanged
- [ ] `/permissions` renders only through `Surface` — no `print!`/`println!` in
      the handler (BR-12; TASK-007 asserts this mechanically)
- [ ] **BR-14 fence**: no `/effort` row, alias, or handler is added by this task
- [ ] `cargo test -p teton` green; no clippy warnings

## Technical Notes

Read `slash.rs`'s module doc before starting — it is unusually explicit about why
`COMMANDS` is the single artifact and why an alias is never a second row.

`/model set` is the precedent for a state-changing command, but its typed-only
refusal does **not** apply here: that restriction exists because `/model set`
changes *machine* state. The permission level is session-scoped and evaporates
with the session (BR-6), and BR-10 requires the read path to work on a pipe, so
`/permissions` is pipe-friendly in both directions like every other command.

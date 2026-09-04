---
id: TASK-002
title: "harness/root_gate.rs — the two pure root decisions"
status: draft
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

The pure, I/O-free policy module both write-capable tools and the shell's result
renderer read (architecture ADR-1). Holds the write-verb table, the quote-aware
top-level-operator scanner, and the cwd-note decision. No tool wiring in this
task — that is TASK-003/004.

## Files to Create/Modify

- `crates/tetond/src/harness/root_gate.rs` — new module.
- `crates/tetond/src/harness/mod.rs` — declare it.

## Acceptance Criteria

- [ ] `write_gate(command, kind) -> WriteVerdict` returns `Allowed` for every
      `kind` of `Project` and `Plain`, whatever the command (BR-4's plain-root
      carve-out, and BR-9).
- [ ] At `Home` / `FilesystemRoot`: refuses when a **command-position** word is
      in `WRITE_VERBS` — so `cd ~ && mkdir foo` refuses, not just a leading
      `mkdir`.
- [ ] At `Home` / `FilesystemRoot`: refuses on a top-level `>`, `>>` or `>|`
      **outside** single quotes, double quotes and backslash escapes — so
      `echo hi > ~/x` refuses and `echo "a > b"` does not.
- [ ] Fail-closed: a non-empty command yielding no command-position program
      refuses at a non-project root.
- [ ] Benign path: `ls -la`, `cat README.md`, `git status`, `echo "2 > 1"` are
      all `Allowed` at a `Home` root.
- [ ] `cd_note(command, root_display) -> Option<String>` returns the BR-2 note
      for a command containing a `cd` command-position word, `None` when the
      command has none.
- [ ] `cd_note` returns `None` when the `cd` target resolves to the session root
      itself (`cd .`, and the root's own literal path), and `Some` when the
      target cannot be resolved statically (`cd "$X"`, `cd $(cat p)`) — the
      fail-toward-emitting direction the spec's assumption fixes.
- [ ] A quote-aware `split_top_level(command, sep)` helper is exported for
      TASK-007's `||` split, so the two scanners are one implementation.
- [ ] `cargo test -p tetond root_gate` passes; every case above is table-driven.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/src/harness/root_gate.rs::the_cd_note_fires_on_a_cd_and_on_nothing_else` | yes |
| BR-2 | test-case | `crates/tetond/src/harness/root_gate.rs::an_unresolvable_cd_target_still_earns_a_note` | no |
| BR-4 | test-case | `crates/tetond/src/harness/root_gate.rs::the_write_gate_refuses_both_triggers_and_nothing_benign` | yes |
| BR-4 | test-case | `crates/tetond/src/harness/root_gate.rs::an_unparseable_command_fails_closed_at_a_non_project_root` | no |
| BR-9 | test-case | `crates/tetond/src/harness/root_gate.rs::a_project_or_plain_root_gates_nothing` | yes |

## Technical Notes

**Reuse `command_position_programs`** from `harness/tools/shell.rs` for trigger
(a) rather than writing a second tokenizer — it already walks `;`, `&&`, `||`
and pipes and strips `VAR=value` prefixes for REQ-607's advisory. Make it
`pub(crate)` if it is not already. Two tokenizers agreeing on ordinary input is
not a property (REQ-563's rule); the adversarial spellings are where they
diverge.

`WRITE_VERBS` is a pinned table, as the spec's assumption asks: `mkdir`, `touch`,
`rm`, `mv`, `cp`, `tee`, `install`, `ln`, and the two-word `git init` (matched on
the program plus its first argument).

The quote scanner is one function used by both the redirection detector and
TASK-007's `||` split. Write it once here.

**Show each test can fail** (conventions.md): break the verb table and the quote
handling, confirm red, and record the mutation in each test's doc comment.

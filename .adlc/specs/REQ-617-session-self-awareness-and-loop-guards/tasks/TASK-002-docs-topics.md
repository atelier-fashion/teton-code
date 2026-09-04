---
id: TASK-002
title: "teton_docs gains `commands` and `transcript`, completes `context` and `skills`, and pays for both names out of the frame"
status: draft
parent: REQ-617
created: 2026-09-04
updated: 2026-09-04
dependencies: ["TASK-001"]
---

## Description

BR-2. Two new bundled topics and two completed ones, with the topic index in the
tool description updated in the same change. Per ADR-3 the description is at 108
of its 120-character ceiling and the two names cost 22, so the frame sentence
pays — the ceiling does not move.

## Files to Create/Modify

- `crates/tetond/src/docs/commands.md` — new. The full roster with effects and
  the `teton` twin where one exists, plus the sentence that only the user runs
  them and the model's job is to name one.
- `crates/tetond/src/docs/transcript.md` — new. The two switches and their
  lifetimes, the directory, and that the model's own tools cannot read it (the
  denied-prefix rule, REQ-611 ADR-7) — which is the fact that stops the next
  seven-tool-call search.
- `crates/tetond/src/docs/context.md` — must name `/context on|off|init` and
  `[context] repo_file`.
- `crates/tetond/src/docs/skills.md` — must name the four load globs and that
  `skill` is the only way the model runs one.
- `crates/tetond/src/harness/tools/docs.rs` — `TOPICS` gains two rows;
  `TOPIC_INDEX` and `DESCRIPTION` gain two names and lose frame bytes; the
  `DESCRIPTION` doc-comment ledger gains this REQ's line.
- `crates/teton/tests/` — AC-2's enumeration (it needs both `slash::COMMANDS`
  and the bundled body, and only the CLI crate sees both).

## Acceptance Criteria

- [ ] `teton_docs commands` names every row of `SESSION_COMMANDS`, asserted by
      enumeration rather than by a golden body.
- [ ] `DESCRIPTION` is ≤ `MAX_DESCRIPTION_CHARS` (120, unchanged) and the
      existing `the_description_indexes_every_bundled_topic` test passes with
      `TOPIC_INDEX` and `TOPICS` agreeing on nine names.
- [ ] `transcript.md` states that tools cannot read the transcript directory.
- [ ] Every topic body is under `MAX_TOPIC_BYTES`; the existing ceiling sweep
      covers the two new ones with no edit (it is a statement about the set).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/src/harness/tools/docs.rs::tests::the_description_indexes_every_bundled_topic` | no |
| AC-2 | test-case | `crates/teton/tests/cli_e2e.rs::the_commands_topic_names_every_registered_command` | yes |

## Technical Notes

The AC-2 enumeration is the point of the whole task: *a new command cannot be
added without its docs line*. Write it against `SESSION_COMMANDS` (TASK-001),
not against a hand-listed set — a hand-listed set is a third table and would
need its own drift guard.

`commands.md` is prose, not generated at build time. Generating it would make
the body unreviewable in a diff, and the enumeration test already supplies the
guarantee generation would have bought.

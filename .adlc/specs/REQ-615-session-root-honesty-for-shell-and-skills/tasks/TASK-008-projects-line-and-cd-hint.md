---
id: TASK-008
title: "The projects tool names the mechanism, and a typed `cd` is offered as `/cd`"
status: draft
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: []
---

## Description

BR-8 (daemon, one line) and BR-7 (client, one pure predicate plus one gated call
site — architecture ADR-7). Grouped because each is small and neither shares a
file with any other task.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/projects.rs` — the trailing line.
- `crates/teton/src/slash.rs` — `cd_as_prompt_hint`, pure.
- `crates/teton/src/main.rs` — the `typed_input`-gated call site in the entry
  loop.

## Acceptance Criteria

- [ ] The `projects` tool's result ends with *"Only the user can run `/cd`. Ask
      them."*, and a mutation deleting the line fails the test (AC-7).
- [ ] `cd_as_prompt_hint` matches BR-7's canonical regex `^cd(\s+\S+)?\s*$`
      against the trimmed line, and returns `None` for everything else —
      including `cd a b`, `cd x && y`, and a line merely starting with `cd`
      (`cdto`).
- [ ] A typed `cd /teton-code` **sends nothing** and prints the BR-7 hint.
- [ ] `//cd /teton-code` sends `/cd /teton-code` as prompt text — the existing
      `Input::EscapedPrompt` path, asserted rather than changed.
- [ ] `printf 'cd x\n' | teton` sends `cd x` to the model: the hint is gated on
      `ctx.typed_input` and a piped session never sees it.
- [ ] `cargo test -p teton && cargo test -p tetond` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-7 | test-case | `crates/teton/src/slash.rs::the_cd_hint_matches_a_bare_cd_and_nothing_else` | yes |
| BR-7 | test-case | `crates/teton/src/slash.rs::the_double_slash_escape_sends_the_slash_command_as_text` | no |
| BR-8 | test-case | `crates/tetond/src/harness/tools/projects.rs::the_listing_names_who_can_run_cd` | no |
| AC-6 | test-case | `crates/teton/src/slash.rs::the_cd_hint_matches_a_bare_cd_and_nothing_else` | yes |
| AC-7 | test-case | `crates/tetond/src/harness/tools/projects.rs::the_listing_names_who_can_run_cd` | no |

## Technical Notes

`cd_as_prompt_hint` is pure and takes no context — the piped exemption is an `if
ctx.typed_input` at the one call site in `main.rs`'s loop, which is where the
flag already lives (`IsTerminal::is_terminal(&stdin())`, read once at the edge).
Do **not** add an `Input` variant to `classify`: `classify`'s value is that it
knows nothing about how a line arrived, and a terminal fact inside it would cost
that.

The hint text is BR-7's, verbatim: *"`cd` is a session command here: `/cd
<path>` moves the root (`/cd` alone shows it). Send as a prompt anyway with
`//cd …`."*

For AC-7's mutation: assert the exact sentence, so deleting it is red. Record the
mutation in the test's doc comment.

---
id: TASK-387
title: "CLI: `/context init [--force]`, `teton context init|generate`, the prompt, the lines, and the banner clause"
status: complete
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-380, TASK-386]
---

## Description

ADR-7 and the surfaces: the slash row and handler, the one-shot shell session for `init`, the
config write for `generate`, rendering of the new permission subject (both questions), the event
lines and the `/verbose` drafting line, the banner/`/cd` clause announcing the coming offer, and
the `NoTerminal` refusal for the new subject.

## Files to Create/Modify

- `crates/teton/src/slash.rs` — `/context init [--force]` row (its own handler beside
  `handle_context`, so `split_name`'s longest match routes the flag), `context_init_on` as the
  body both doors run, `send_context_action` as the one call site, and `render_context`'s
  `origin` clause.
- `crates/teton/src/main.rs` — `teton context init [--force]` (one-shot session at cwd) and
  `teton context generate ask|always|never`; the two doctor advisories; the banner clause's
  call site.
- `crates/teton/src/session_ui.rs` — subject rendering (`replace` wording), `refusal_line` arm,
  event lines and the drafting line, and the two `SessionState` fields the launch clause reads.
- `crates/teton/src/banner.rs` — `generation_notice`, the launch clause itself, beside
  `root_notice` whose complement it is.
- `crates/teton/src/status.rs` — the posture line's `generate` clause and the two advisories.
- `crates/teton/src/cli_rows.rs` — README cross-check rows; `SHELL_ONLY` gains
  `context generate`; `refusal_for_path` gains its sentence.
- `crates/teton-protocol/src/methods.rs` + `crates/tetond/src/runtime/views.rs` — additive
  `RepoContextPosture::generate`, without which the doctor line and the launch clause have no
  posture to read (see Deviations).

## Acceptance Criteria

- [x] AC-3: on a pipe at `guarded` the client refuses without reading stdin (the next line is
      still the next prompt), prints one line, no file; with `generate = always` on the same
      pipe the file is written.
- [x] AC-10: `/context init` with a file refuses naming size and `--force`; `--force` prompts
      with the replace wording and, accepted, replaces; `teton context init` produces the same
      bytes as the session door on the same fixture.
- [x] The banner prints the announcement clause only when the daemon reports `absent` and
      generation is not suppressed; `/verbose` prints the drafting line with tier, entries and
      tokens.
- [x] `teton context generate never` writes the key through `config/set`; the doctor posture
      line shows it.
- [x] `cargo test -p teton --no-fail-fast` green, including the `cli_e2e` legs named below.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead` | yes |
| BR-8 | test-case | `crates/teton/tests/cli_e2e.rs::context_init_refuses_without_force_and_the_shell_door_writes_the_same_bytes` | yes |
| BR-10 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead` | yes |
| AC-3 | test-case | `crates/teton/tests/cli_e2e.rs::a_piped_session_refuses_the_generation_offer_without_reading_stdin_and_always_writes_instead` | yes |
| AC-10 | test-case | `crates/teton/tests/cli_e2e.rs::context_init_refuses_without_force_and_the_shell_door_writes_the_same_bytes` | yes |

## Technical Notes

The one-shot session uses the same root probe and level as `teton`; on a pipe it inherits the
`NoTerminal` refusal. Never write config from the slash handler (REQ-611 BR-2's split).

## Verification run (2026-09-03)

`cargo test -p teton --no-fail-fast` 765 + 82 + 23 green; `cargo test -p teton-protocol`
228 green; `cargo test -p tetond` green; `cargo clippy -p teton -p tetond -p teton-protocol
--all-targets -- -D warnings` clean; `cargo check --workspace --all-targets` clean;
`cargo fmt --all -- --check` clean.

Nineteen mutations were run and every one reddened the test that claims it; each is recorded in
the doc comment of the test it belongs to. The set: the offer answered on a pipe; `refusal_line`
losing its arm; one question rendered for both spellings of `replace`; the offer dropping its two
costs; `written` moved behind the verbose gate; `offered` printed unconditionally; the drafting
line dropping the entry count; every terminal stage worded the same; the event arm forgetting the
state; the launch clause dropping the project gate and dropping the `never` gate; an unreported
posture rendered as `ask`; the posture clause worded as prose; the one-shot session dropping its
`cwd`; the `Failed` line dropping the daemon's reason; `teton context generate` writing no
`ConfigUpdate`; the `always` advisory moved into the session-scoped pass; and the `never`
advisory's posture guard inverted.

## Deviations

1. **`/context init` is its own `CommandSpec` handler, not a branch of `handle_context`.**
   `split_name`'s longest match routes `/context init --force` to a `context init` row before
   `handle_context` ever sees it, so an `init` arm inside that function's argument match would be
   unreachable — and the row has to exist under that exact spelling anyway, because
   `cli_rows::readme_tests` compares README rows against `COMMANDS` *names*. What the two share is
   `send_context_action`: one call site of `session/context`, one renderer. `handle_context`'s
   unknown-argument line names `/context init [--force]`.
2. **`RepoContextPosture` gained an additive `generate` field** (protocol + `views.rs`), which is
   outside the task's file list. The AC asks the doctor posture line to show `generate` and the
   banner clause to be drawn only where generation is not suppressed, and nothing on the wire
   carried the posture: `SessionContextResult::generation` reports what a *call* did, and
   `RepoContextState` reports the file. The field is `Option`, `skip_serializing_if`, and reads as
   "not reported" on an older daemon.
3. **The launch clause cannot distinguish an empty `TETON.md` from no file at all.** The wire
   folds both into `RepoContextState::Absent` deliberately ("no surface has a different remedy for
   them"), so `touch TETON.md` earns the clause and then no prompt. The AC's own wording is "when
   the daemon reports `absent`", which is what is implemented; the alternative — going quiet
   wherever the client is unsure — would suppress the clause in the ordinary case it exists for.
   Recorded in `banner::generation_notice`'s doc.
4. **Two pre-existing collisions with TASK-386's offer were repaired in tests, not weakened.**
   `pty_e2e::the_acknowledgment_prompt_names_the_root_its_skills_and_what_it_left_out` was already
   red on this branch before this task (the fixture root drew a second permission question at one
   terminal); its project now carries an empty `TETON.md`, which is BR-1's documented way to say
   "not here". `cli_e2e::slash_verbose_toggles_the_route_notice_around_real_turns` asserted on the
   bare `context: ` label as a proxy for "nothing was clamped"; REQ-613 gave that label a second,
   ungated family, so the needle is now the five clamp sentences themselves — narrower, not
   weaker.
5. **`answer_permissions: true` on the one-shot `init` context is covered only through the pipe
   refusal.** A leg that exercised it at a terminal would need a pty fixture for
   `teton context init`; the piped leg does prove the flag is live, because a context that
   answered nothing would leave the daemon's gate waiting rather than printing a refusal.

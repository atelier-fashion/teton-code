---
id: TASK-173
title: "e2e: read parity, writes, piped refusals, /help, presence; CHANGELOG, README, runbook, spec ticks"
status: complete
parent: REQ-582
created: 2026-08-18
updated: 2026-08-18
dependencies: [TASK-170, TASK-171, TASK-172]
repo: teton-code
---

## Description

Close the acceptance criteria end to end over real binaries and write the
user-facing docs. AC-1: for each read row drive `teton <sub>` and a piped
session `/<sub>` against one scripted daemon and diff the lines (session
output from the session-ready line onward; `/doctor` minus the connect arm).
AC-2: under `TETON_TEST_SEAMS=1` (`run_cli_seamed`), `/policy set-tier build
<id>`, `/policy set-category edit <id> --fallback <id>`, `/boundary add
<glob> --mode local-only`, then `teton policy show` / `teton boundary list`
reflect them. AC-3: pty test — `/provider add …` reads the key echo-off
(REQ-579's keychain seam / MockKeychain path from `provider_setup` pty
tests), key absent from the transcript, config carries `keychain://`. AC-4:
each write row on a pipe prints the shell pointer; reads work. AC-8: `/help`
over a pipe lists every row. AC-11: on a `presence`-featured build with
`TETON_PRESENCE_ACCEPT=fail`, `/policy set-tier` is refused and `config.toml`
is byte-identical, paired with the accept seam (feature-gated test; skip
cleanly otherwise). Docs: CHANGELOG `[Unreleased]` entry, README session
section, `docs/manual-verification.md` runbook (dogfood the screenshot flow:
ask to test Kimi, type `teton provider list`), tick the spec's AC boxes with
evidence, mark tasks complete.

## Files to Create/Modify

- `crates/teton/tests/cli_e2e.rs` — AC-1, AC-2, AC-4, AC-8, AC-5/6 (if not landed in TASK-170), AC-11 (feature-gated).
- `crates/teton/tests/pty_e2e.rs` — AC-3 (`/provider add` echo-off; keychain seam).
- `CHANGELOG.md` — `[Unreleased]` → Added.
- `README.md` — session commands section.
- `docs/manual-verification.md` — REQ-582 dogfood check.
- `.adlc/specs/REQ-582-run-cli-commands-from-the-session/requirement.md` — AC ticks with pointers to tests.

## Acceptance Criteria

- [x] AC-1..AC-4, AC-8, AC-11 covered by tests named for their AC; `cargo test --workspace --no-fail-fast` shows no `FAILED` (grep — LESSON-533).
- [x] `git diff origin/main...HEAD -- crates/teton-protocol/src/` is empty (AC-12) — run, zero lines.
- [x] CHANGELOG entry follows the file's style (what an upgrade changes for a running machine; the screenshot story in one paragraph).
- [x] fmt/clippy clean.

## Outcome

Tests added:

| AC | Test |
|---|---|
| AC-1 | `cli_e2e.rs::every_read_row_prints_exactly_what_its_shell_twin_prints` |
| AC-2 | `cli_e2e.rs::the_write_rows_change_the_config_their_shell_twins_read_back` |
| AC-3 | `pty_e2e.rs::a_session_provider_add_asks_for_its_key_echo_off_and_stores_nothing_untyped`, `cli_rows.rs::provider_add_reads_its_key_through_the_hiding_prompt_and_never_as_a_flag` |
| AC-4 | `cli_e2e.rs::on_a_pipe_every_write_row_names_its_shell_twin_and_changes_nothing` |
| AC-8 | `cli_e2e.rs::slash_help_lists_every_mirrored_row_grouped_with_both_footers` |
| AC-11 | `cli_e2e.rs::a_presence_refused_session_set_tier_leaves_the_config_untouched`, `cli_e2e.rs::an_attested_session_set_tier_writes` |

Harness additions: `TestDaemon::spawn_scripted_with_presence` (one more env pair
on the fixture), `typed_output` (a typed line's own output, taken out of the
entry-prompt frame), `attach_lines` (the lifecycle replay every client run
receives, so it can be removed from a parity diff by identity rather than by
shape), `command_lines`.

### Deviations

1. **AC-3 is covered with a documented gap, and the gap is a rule rather than
   an omission.** No test types a credential into `/provider add`. The flow
   stores through `keychain::default_keychain()` — the real login keychain on
   macOS — with no seam to redirect it and no confirm step between the read and
   the store, so a completed walk would create (and, on a rejected
   registration, delete) an entry in whoever's keychain ran the suite. That is
   the rule `pty_e2e.rs` already records for `/web setup`'s key step, and
   adding a redirect seam would mean shipping a build that can be talked into
   writing a plaintext secret elsewhere. What the pty test asserts instead: the
   row runs at a TTY, reaches the credential step, refuses on an empty answer
   with `config.toml` byte-identical and nothing registered, and — the echo
   claim, made **fail-closed** — reached the read at all without printing
   `ECHO_UNAVAILABLE`, which under a pty is only possible with `ECHO` cleared.
   A `--kind local` registration in the same session goes end to end, so
   "nothing changed" above is not the answer of a row that cannot write. The
   unit test pins that the question went through `ask_secret`, that it was the
   row's only question, and that `--key` does not parse. The sweep of a real
   typed credential over a real terminal stays
   `pty_e2e.rs::the_key_step_does_not_echo_and_the_key_reaches_nothing`, over
   the same `Prompter::ask_secret` seam.
2. **AC-11 is not feature-gated, and does not need to be.** The task file
   expected `#[cfg(feature = "presence")]` or a runtime posture probe.
   `TETON_PRESENCE_ACCEPT` installs a verifier *in place of* whatever the build
   has (`tetond::attest::seam_verifier`), so a default build driven through it
   takes the same `config/set` path a `--features presence` build takes with a
   real mechanism — which is exactly how `tetond/tests/config_set_attestation.rs`
   already drives this gate, also ungated. So the pair runs on every CI build
   rather than skipping on most of them, which is strictly better than the
   "skip cleanly with a printed reason" fallback the task allowed for.
3. **The AC-1 parity diff removes the attach replay by identity.** Since
   BUG-177 the daemon replays the model lifecycle to whichever connection is
   attaching, so a *shell* run's stdout also carries `>> probe: …` /
   `>> local model … ready` — and not in one place: `doctor` prints its header
   before the `config/get` those frames are drained by, so they land inside its
   report while `provider list` sees them ahead of one. They are removed by
   matching the session's own attach lines, each exactly once, with the
   leftovers asserted empty — rather than by shape (`>> `), which would also
   swallow `/doctor`'s two trailer notices and `provider list`'s base-URL
   advisory.

## Technical Notes

- Build the workspace before targeted e2e runs (`cargo build --workspace`) — LESSON-510/BUG-164.
- Byte-diff parity: compare `LineKind`-agnostic text; the session prints `>> ` prefixes for notices — compare the same surface class the shell prints (both use `stdout_surface`), so bytes should match exactly; if a legitimate prefix difference exists, assert on the text after the prefix and say why in the test.
- The presence test needs `--features presence`; gate with `#[cfg(feature = "presence")]` or a runtime check on the built binary's posture, mirroring how REQ-576's e2e is gated (`crates/tetond/tests/config_set_attestation.rs`).

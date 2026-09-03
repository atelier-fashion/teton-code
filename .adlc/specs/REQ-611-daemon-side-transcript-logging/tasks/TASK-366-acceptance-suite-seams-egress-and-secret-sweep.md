---
id: TASK-366
title: "Acceptance: the transcript e2e suite, the two test seams, egress capture, and the secret sweep"
status: complete
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-364, TASK-365, TASK-368]
---

## Description

The end-to-end evidence, in one new integration file plus the four existing suites that must
learn the new surface. Adds the two `TETON_TEST_SEAMS`-gated seams the spec's AC-9 and AC-10 need
(force the sink channel full; make the transcript dir unwritable), the egress-capture proofs for
AC-12 and AC-21, the REQ-572 AC-5 secret sweep extended to the transcript directory (AC-14), and
the daemon-scoped leak enumeration in `multi_client.rs`. Covers AC-1, AC-9, AC-12, AC-14, AC-21
and the e2e legs of BR-8 and BR-11.

## Files to Create/Modify

- `crates/tetond/tests/transcript.rs` — new; uses `e2e/harness.rs` (`Daemon::spawn`,
  `MockProvider`, `Client`, `global_capture`). Cases for AC-1, AC-9 and the cross-cutting checks
  the earlier tasks reference by name.
- `crates/tetond/src/runtime/engine.rs` (or the seams module it re-exports) — two seams behind
  `test_seams_enabled()`: `TETON_TRANSCRIPT_SEAM=channel_full` (sink channel capacity 1) and
  `=dir_unwritable_after:<n>` (chmod the dir `0o500` after `n` records). A release build refuses
  both like every seam.
- `crates/tetond/tests/egress_capture.rs` — AC-12: `read`, `edit`, `grep` and `glob` aimed at
  the session's own transcript are each refused with the transcript reason, for a dir outside the
  root and for one inside it; a `shell` `cat` of the file succeeds and the next remote-routed
  prompt is blocked at egress, with `assert_no_boundary_bytes()` extended to the transcript's
  bytes. AC-21: the four refusals hold unchanged with `disable_default_boundaries = true`, paired
  with an in-root `.ssh/id_rsa` fixture read that is now **not** blocked at egress.
- `crates/teton/tests/pty_e2e.rs` — the REQ-572 AC-5 sweep gains the transcript directory: after
  the setup flows run with the transcript on, grep every `*.jsonl` under the dir for the fixture
  key bytes.
- `crates/tetond/tests/multi_client.rs` — add `transcript_state` to the session-scoped set in the
  daemon-leak test and assert a monitor receives it while a bystander does not.
- `crates/tetond/tests/config_preservation.rs` — an unrelated `config/set` on a file that names
  `[transcript]` preserves it byte-for-byte; one that never named it does not gain it.

## Acceptance Criteria

- [x] AC-1: a stock-config session (prompt, tool call, reply, exit) leaves no `transcripts`
      directory under the data dir; asserted by listing the filesystem after the daemon exits.
- [x] AC-9: with `channel_full`, a turn that publishes ≥ 50 envelopes completes without delay
      (bounded by the same timeout the suite uses for an ordinary turn), and the file's
      `transcript_gap.dropped` equals `published - written`, with `n` contiguous.
- [x] AC-12: all four file tools are refused, inside and outside the root; the shell leg's egress
      capture holds no transcript bytes; the test goes red when the denied prefix is removed from
      `ToolContext` **or** from `WalkPolicy` (two mutations, both recorded in the doc comment —
      LESSON-502).
- [x] AC-21: with defaults disabled, the four refusals are unchanged and the `id_rsa` read is no
      longer blocked — both legs on one fixture.
- [x] AC-14: the sweep finds zero occurrences of the fixture key in the transcript directory;
      the sweep is shown to fire by planting the key in a throwaway `.jsonl` first (mutation).
- [x] AC-8 enumeration: `multi_client.rs` lists `transcript_state` and the leak test stays green.
- [x] Both seams panic in a release build when set (the existing seam contract) — one test per
      seam under `cfg(not(debug_assertions))`.
- [x] `cargo test --workspace --no-fail-fast` green on macOS and the ubuntu CI leg; fixtures do not
      depend on directory listing order (LESSON-540).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/tests/transcript.rs::a_stock_config_writes_nothing_and_constructs_no_sink` | yes |
| BR-8 | test-case | `crates/tetond/tests/transcript.rs::every_file_tool_refuses_the_transcript_and_shell_output_is_held_at_egress` | yes |
| BR-11 | test-case | `crates/tetond/tests/transcript.rs::the_redactor_is_not_installed_by_the_transcript_and_the_opened_record_states_posture` | yes |
| AC-1 | test-case | `crates/tetond/tests/transcript.rs::a_stock_config_writes_nothing_and_constructs_no_sink` | yes |
| AC-8 | test-case | `crates/tetond/tests/multi_client.rs::a_transcript_state_reaches_the_attached_client_and_never_a_bystander` | yes |
| AC-9 | test-case | `crates/tetond/tests/transcript.rs::a_full_sink_channel_delays_nothing_and_writes_one_gap` | yes |
| AC-10 | test-case | `crates/tetond/tests/transcript.rs::the_write_fail_after_seam_degrades_the_session_once_and_spares_the_turn` | yes |
| AC-12 | test-case | `crates/tetond/tests/transcript.rs::every_file_tool_refuses_the_transcript_and_shell_output_is_held_at_egress` | yes |
| AC-14 | test-case | `crates/teton/tests/pty_e2e.rs::the_fixture_key_never_reaches_the_transcript_directory` | yes |
| AC-19 | test-case | `crates/tetond/tests/config_preservation.rs::a_transcript_table_survives_an_unrelated_write_and_is_never_invented` | yes |
| AC-21 | test-case | `crates/tetond/tests/transcript.rs::the_transcript_refusal_survives_disable_default_boundaries_while_id_rsa_does_not` | yes |

## Technical Notes

Golden-sequence discipline (LESSON-591): the file's order is asserted only by the three order
relations in AC-2; never pin `cost_recorded`'s position, it is published from the cost task.

For AC-9 count `published` from the mock provider's scripted turn plus the tool events, not from
the file — the file is the subject.

Seams can only deny or degrade, never grant (LESSON-519's rule); neither seam may turn the
transcript on. Route both through `test_seams_enabled()` so a release build with the variable set
panics at startup as the others do.

## Outcome

- Implemented in-context by the orchestrator after the dispatched agent was
  killed by a backend 529 before writing anything.
- **AC-12 and AC-21 live in `crates/tetond/tests/transcript.rs`, not
  `egress_capture.rs`** (Verification rows corrected above): the four tool
  refusals and the shell leg need a real daemon, scripted tool calls and the
  e2e `MockProvider` capture, which is the transcript suite's harness;
  `egress_capture.rs` is a unit-level `Egress` harness with no tools.
- The two seams are `TETON_TRANSCRIPT_SEAM=channel_full` (sink channel of one
  slot) and `TETON_TRANSCRIPT_SEAM=write_fail_after:<n>` (the `n+1`th append
  fails), both parsed by the pure `transcript::parse_seam` and honoured only
  where `engine::test_seams_enabled()` says so. The release-build refusal is
  therefore the shared `TETON_TEST_SEAMS` policy (which panics on a release
  build before the parser is consulted); it is pinned by a table test on the
  parser rather than by `cfg(not(debug_assertions))` tests, which CI never
  compiles. The task's `dir_unwritable_after` spelling was replaced: directory
  modes do not fail writes to an already-open file, so the seam arms the
  writer instead.
- The existing AC-10 e2e leg (TASK-364) induces the failure by replacing the
  file with a directory; the new seam test covers the mid-session arm.
- Two fixture lessons the AC-12 test now documents in code: tool-call
  arguments and grep's "no matches for `…`" line both echo into the
  conversation and reach the provider legitimately, so the egress marker may
  live only in the file's bytes.
- Mutations run (all red, all restored): jail seam removed (read/edit leg),
  walker seam removed (grep leg), planted key in a throwaway `.jsonl` (sweep
  fires), `note_dropped` no-op (AC-9), publish with `None` (bystander leg),
  `RegisterProvider` touching `[transcript]` (preservation), `redact: true`
  hard-coded (BR-11), stock test run under `enabled = true` (AC-1), runtime
  fault arm neutralised (seam test). Dropping `skip_serializing_if` alone
  stays green, for the structural reason recorded in the test's doc comment.
- Full workspace: 4170 tests green, clippy clean.


---
id: TASK-041
title: "PTY harness and the timing e2e that today's piped suite structurally cannot run"
status: draft
parent: REQ-556
created: 2026-08-04
updated: 2026-08-04
dependencies: [TASK-040]
---

## Description

AC-2 is the headline regression test for the defect that motivated REQ-556 — the
`ready` line arriving on its own, with no prompt typed — and it is *about
timing at a terminal*. BR-2 makes the indicator emit nothing on a pipe, and
`cli_e2e` drives the binary over pipes (`crates/teton/tests/cli_e2e.rs:231`), so
there is no existing route to this behaviour. This task adds the one.

The dev-dependency is declared in the requirement's External Dependencies
deliberately, so "the ACs are untestable" surfaces here rather than as a quietly
dropped criterion.

## Files to Create/Modify

- `crates/teton/Cargo.toml` — pty crate as a `[dev-dependencies]` entry only
- `crates/teton/tests/pty_e2e.rs` — **new**: spawn `teton` under a pty against the scripted test daemon; assert AC-2 and AC-5

## Acceptance Criteria

- [ ] AC-2: with the tier opening mid-session, `>> local model <id> ready`
      appears in the pty transcript **with no line written to the pty's input**.
      The test must fail against a binary without TASK-038 — verify that
      explicitly by stashing the change or by asserting on a recorded
      pre-REQ transcript, not by assuming.
- [ ] AC-5: a line typed while the indicator is running arrives at the daemon
      intact and the entry frame is not corrupted in the transcript.
- [ ] The harness tolerates timing without being flaky: assert on *state
      reached* (the line appeared) with a generous bound, never on a fixed
      sleep (LESSON-450 — synchronize on the state-derived surface, not on a
      guessed interval).
- [ ] The pty crate is a **dev-dependency only** — `cargo tree --edges normal`
      shows it absent from the shipped binary's graph.
- [ ] Skips cleanly (not fails) where no pty is available, in the same style as
      `daemon_or_skip()` in `cli_e2e.rs`.
- [ ] AC-7: the new test runs on both macOS and Linux in CI. A green macOS run
      is not evidence about Linux (LESSON-433).

## Technical Notes

- `TestDaemon::spawn_scripted` and `daemon_or_skip()` in `cli_e2e.rs` are the
  existing harness primitives — reuse them for the daemon side and add only the
  pty on the client side.
- The scripted daemon reaches `ready` quickly. To exercise the *loading window*
  the test needs the tier to be visibly not-ready for a bounded period; check
  whether the existing `TETON_FAKE_ENGINE_LOADER` seam (used by
  `crates/tetond/tests/e2e/consent_matrix.rs`) can hold it there. **If no
  existing seam can, say so and record it** — do not invent a production-code
  delay to make a test pass (Ethos 6).
- Assert on the pty transcript with ANSI intact or stripped, but be explicit
  about which; the indicator's repaint emits cursor control that a naive
  substring assert will trip over.
- Do not weaken `cli_e2e`'s piped byte-equality tests to accommodate anything
  here — they are the AC-4 guarantee and must stay unmodified.

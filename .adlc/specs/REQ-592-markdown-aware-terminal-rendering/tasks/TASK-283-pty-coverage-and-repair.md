---
id: TASK-283
title: "Pty coverage for the rendered bytes, and repair of the assertions this REQ moves"
status: complete
parent: REQ-592
created: 2026-08-26
updated: 2026-08-26
dependencies: [TASK-280, TASK-281]
---

## Description

The only harness that can see this feature at all. Adds the three pty legs the ACs name, and
repairs the existing pty assertions BR-3 moves (ADR-8).

## Files to Create/Modify

- `crates/teton/tests/pty_e2e.rs` — AC-8 colour leg, AC-10 tail-flush leg, AC-12 rendered bytes;
  plus re-verification of existing assistant-text assertions.

## Acceptance Criteria

- [ ] AC-12: at a fixed pty width, a scripted turn's **rendered bytes** show wrapped rows, a
      transposed table, and one SGR-styled span. Written here, not claimed as covered elsewhere.
- [ ] AC-8 (pty leg): a session launched with `NO_COLOR=1` in the child environment produces
      wrapped rows and **no escape sequences**.
- [ ] AC-10 (pty leg): a reply whose final chunk lacks a trailing newline has its last row visible
      above the entry frame, before any hand-off line.
- [ ] Every existing pty test passes. Where BR-3's wrapping split a marker, the fix is to widen
      `cols` or split the assertion — **and the comment at pty_e2e.rs:894 is updated** to say the
      wrap is now the CLI's, not the terminal's.
- [ ] `cargo build --workspace` precedes the run (BUG-164 staleness guard).

## Technical Notes

**Why existing assertions move.** A terminal's hard wrap is a *display* artifact — the pty master
receives the bytes the CLI wrote, so today a long assistant line reaches the transcript contiguous.
BR-3 inserts **real `\n` bytes**. Any assertion matching a contiguous assistant-text substring
longer than the pty width starts failing.

Bounded: `cli_e2e` is piped and inert (BR-7); assertions on `line()`-kind output are untouched
(OQ-5 leaves those unwrapped). The exposure is pty tests whose *assistant* text exceeds their
`cols`. Start with the five at `cols: 100` (pty_e2e.rs:279, 393, 494, 692, 1247) and
`a_reply_reciting_the_cli_earns_the_hand_off_line_at_a_terminal` (:876, a 78-char paragraph at
200 cols — highest-risk, expected to survive, must be confirmed rather than assumed).

Harness shape is established: `TestDaemon::spawn_with_env` with a scripted engine
(`TETON_LOCAL_SCRIPT`, `---`-separated replies), `wait_for_socket` before any client (BUG-164),
`PtySize { rows, cols }` per test, `CommandBuilder::env` for `NO_COLOR`, reader thread plus
`wait_for` marker polling under the 60s `WINDOW` (BUG-173), never a fixed sleep ([[LESSON-450]]).
Assert raw escape bytes directly where geometry is the claim — pty_e2e.rs:556 already does
(`framed.contains("\x1b[3A")`).

Script the turn rather than relying on a real model: AC-12 is a claim about the renderer, and
TASK-282's clause changes what a live model writes ([[LESSON-481]] — pay for the harness the gate
demands, and say so where a gap remains).

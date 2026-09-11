---
id: BUG-225
title: "A line submitted while the activity row animates leaves the row's last frame in scrollback"
status: resolved
severity: low
created: 2026-09-10
updated: 2026-09-10
resolved: 2026-09-10
component: "cli"
domain: "harness"
stack: ["rust", "cli"]
concerns: ["developer-experience"]
tags: ["activity-row", "canonical-mode", "echo", "raw-mode", "scrollback", "req-621"]
introduced_by: ["REQ-621"]
attribution: manual
---

## Description

REQ-621's activity row is repainted in place one row above the cursor. The
terminal stays in canonical mode with echo on during a turn, so a line the user
types is echoed at the cursor and, when Enter is pressed, the cursor drops a
row under bookkeeping the client cannot observe. Left alone, the next repaint
would overwrite the echoed line (BR-9). The shipped mitigation (`RowState::abandon`,
ADR-621-3 as amended 2026-09-10) stops all painting the moment a submitted line
is waiting on stdin, so the typed line is never overwritten and delivery is
intact — at the cost of the row's last frame staying in scrollback for that
turn, a bounded exception now recorded in BR-5. Accepted by the user on
2026-09-10 (REQ-621 Phase 5, option 2) with this bug as the follow-up.

## Reproduction Steps

1. `teton` at a terminal; send a prompt whose reply takes a few seconds.
2. While the row animates, type a line and press Enter.
3. Let the turn finish.

## Expected Behavior

Scrollback after the turn is byte-identical to a turn with nothing typed
(BR-5), and the typed line is delivered intact (BR-9).

## Actual Behavior

The typed line is delivered intact and nothing overwrites it, but the row's
last frame (e.g. `⠙ waiting on local (base) · 2s · turn 2s`) remains in
scrollback above the reply.

## Environment

- Platform: macOS/Apple Silicon (any TTY)
- Version: teton main after REQ-621

## Root Cause

A canonical-mode client has no way to see the cursor move when the kernel
echoes a newline, so the row's geometry is lost the instant a line is
submitted. The real fix is raw-mode input handling for the interactive session
(track typed bytes client-side, echo them itself, and erase the row cleanly),
which is a separate REQ-sized change; REQ-556's one-reader-of-stdin rule
(ADR-556-1) must survive it.

## Resolution

**Fixed by REQ-622 (BR-3, BR-4), 2026-09-10 — the cause removed, not the
symptom.** The root cause named above was right: a canonical-mode client cannot
see the cursor move when the kernel echoes a newline. REQ-622 stops the kernel
echoing. While a turn runs at a terminal the client takes the terminal out of
canonical mode, reads the keystrokes itself, and echoes the pending line on a
row it owns beneath the activity row — so no repaint or withdraw can land on
what the user typed (BR-3, retiring REQ-621 BR-9's submitted-line clause) and
the row is erased cleanly on every turn exit whether or not anything was typed
(BR-4, retiring REQ-621 BR-5's first recorded exception). Enter queues the line
as the next prompt rather than handing it to the kernel's buffer, which also
closes the older hazard the row made visible: a question opened mid-turn can no
longer be answered by type-ahead the user never saw (BR-5).

REQ-556's one-reader-of-stdin rule (ADR-556-1) survives it, as the root cause
required: there is still exactly one reader, and it is the client rather than
the kernel's line discipline.

Commits (branch `feat/REQ-622-client-owned-input-during-a-turn`):

- `669ba28` — `RawMode` guard, one process-wide restore slot, and a `sigaction`
  handler that restores and re-raises (TASK-416)
- `5e1d86e` — the pure input editor: keystroke decoding, the pending row,
  shelve/unshelve, and the queue (TASK-415)
- `55107e1` — the pump engages raw mode, reads keystrokes on its tick, owns two
  rows, and shelves the pending line around questions (TASK-417)
- `8c472fe` — questions read through the editor in raw mode, and queued lines
  re-enter at the entry poll (TASK-418)
- `e654e2a` — the pty legs: type-ahead, questions, every restore path,
  multi-byte and unhandled keys, queued prompts (TASK-419)

The regression leg is
`crates/teton/tests/pty_e2e.rs::a_submitted_line_is_never_overwritten_and_becomes_the_next_prompt`,
which asserts the **opposite** of REQ-621's deleted
`typed_bytes_survive_the_animation` over the same script: the row must go on
animating past a submitted line, and the replayed screen after the turn must
carry no activity glyph at all.

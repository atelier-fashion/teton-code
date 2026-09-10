---
id: BUG-225
title: "A line submitted while the activity row animates leaves the row's last frame in scrollback"
status: open
severity: low
created: 2026-09-10
updated: 2026-09-10
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

(open — follow-up to REQ-621)

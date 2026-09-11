---
id: LESSON-660
title: "A pty session leader cannot prove a restore; a live row must hold what the renderer holds; a flush that is right in raw mode eats the line in canonical mode"
component: "cli"
domain: "harness"
stack: ["rust", "cli"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["raw-mode", "termios", "pty-testing", "session-leader", "live-row", "held-line", "tcflush", "canonical-mode", "worktree-coordination", "verify-panel"]
req: REQ-622
created: 2026-09-10
updated: 2026-09-10
---

## What Happened

REQ-622 took the terminal out of canonical mode for the length of a turn so the
client owns its input. Four things surfaced late, all invisible to unit tests
and two of them invisible to the pty suite as it stood.

1. **Every "terminal restored" assertion was unfalsifiable.** `portable_pty`
   spawned the client as the pty's session leader, and a BSD kernel revokes a
   controlling terminal whose leader exits, handing back driver defaults. The
   first mutation of the signal handler (restore removed) stayed green. Running
   the client under a shell that keeps the session open — the shape a real
   terminal has — made the same mutation redden four legs.
2. **A live row drawn through the durable verb split a streamed reply one
   chunk per row.** `Surface::line` flushes the markdown surface's held partial
   line before writing, so redrawing the pending row after every token ended
   the reply's open row each time. The row's own verbs (`draw_row`,
   `draw_current_row`, and repaint/withdraw variants) must hold what the
   renderer holds; the held line goes out where the row was, on the next durable
   write.
3. **A kernel input flush that is correct in raw mode destroyed the user's line
   in canonical mode.** `tcflush(TCIFLUSH)` at the shelve closes BR-5's window
   during a raw turn — but the question dispatcher is shared with the idle
   drain, where the kernel's queue *is* the unsubmitted line at the entry
   prompt. Step D caught it; the flush is now gated on the pump owning the input.
4. **A second session worked in the pipeline's worktree.** A chip spawned from
   a verify finding was started by the user and implemented the same fix in the
   same files the fix pass was about to edit. Coordinating through a session
   message, waiting for it to go quiet, verifying and adopting its commit, then
   building on it avoided a collision — but only because it was noticed before
   the fix agents were dispatched.

## Lesson

- A restore assertion is only evidence if the process that failed to restore
  would leave the terminal changed: never test terminal-mode restoration with
  the subject as the pty's session leader.
- Any transient row that coexists with streamed text needs surface verbs that
  do not flush the renderer's pending buffer; `line()` is for durable rows.
- When a fix is added at a seam shared by two terminal modes, prove the
  benign mode explicitly (LESSON-440): the must-not-fire case here was the
  canonical idle drain.
- Before dispatching a fix pass, check `git status` in the pipeline worktree
  and `list_sessions`; a running peer editing the same files is a collision,
  not noise. Adopt its commit with attribution rather than racing it.

## Why It Matters

The raw-mode window exists for the length of every turn, so a restore that
silently fails leaves the user's terminal unusable after every Ctrl-C, and a
flush that eats a typed line does so on the most common path there is. Both
were green under the suite as first written.

## Applies When

- Terminal-mode transitions, signal-safe restoration, and pty legs that assert
  on termios.
- Any in-place row painted over a streaming markdown surface.
- Shared dispatch code reached from both raw and canonical input paths.
- Any `/proceed` run where a spawned background chip targets the pipeline's
  own worktree.

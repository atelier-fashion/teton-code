---
id: TASK-038
title: "Unified input+event channel: the interactive entry loop stops blocking on stdin"
status: draft
parent: REQ-556
created: 2026-08-04
updated: 2026-08-04
dependencies: []
---

## Description

Move the interactive entry loop's blocking point off `io::stdin().read_line`
and onto the connection's own channel, so daemon events render while the user
sits idle at the prompt (BR-1). This is the foundation task — every other piece
of REQ-556 is decoration on top of it, and it delivers user-visible value on its
own: `>> local model <id> ready` starts landing when it happens instead of at
the next turn.

Per ADR-556-1: add a stdin variant to `Incoming` and a stdin reader thread that
feeds the *same* `mpsc::Sender` the socket reader already uses. The loop then
blocks in one place on one channel.

**The piped path must not be touched.** Non-TTY sessions keep calling
`FramedStdinPrompter::ask` exactly as today. That is what makes AC-4's
byte-identity true by construction rather than by careful matching.

## Files to Create/Modify

- `crates/teton/src/client.rs` — add `Incoming::Stdin(StdinLine)` with `Line(String)` / `Eof`; add a `spawn_stdin_reader` producer cloning the existing `Sender`; expose a `recv_timeout`-based entry point for the loop to drive
- `crates/teton/src/main.rs` — restructure the interactive branch of `run_session` around the unified channel; leave the non-interactive branch on the existing blocking prompter

## Acceptance Criteria

- [ ] In an interactive session, a `model_lifecycle` event arriving while no
      RPC is in flight renders at the time it arrives (BR-1).
- [ ] A typed line reaches the existing `slash::classify` → dispatch →
      `prompt_turn_params` path byte-identically to today (REQ-555 AC-7 still
      passes unmodified).
- [ ] EOF (Ctrl-D) leaves through the same post-loop cost-summary path it uses
      today; `/quit` and `/exit` still return `CommandOutcome::Quit` (BR-4).
- [ ] The non-TTY path does not construct the stdin thread or the unified loop
      at all — assert this structurally, not by output comparison alone.
- [ ] `slash_quit_ends_the_session_exactly_as_ctrl_d_does` and the `/exit`
      equivalence leg pass **unmodified** (AC-4).
- [ ] Unit test: the channel merge is exercised without a socket or a terminal —
      feed both variants into a `Receiver` and assert the loop's dispatch.

## Technical Notes

- `Incoming` is at `client.rs:89`; `Connection` holds `incoming: Receiver<Incoming>`
  at `client.rs:99`, fed by `reader_loop` spawned in `connect` (`client.rs:117`).
  The second producer clones that `Sender`.
- `recv_timeout` returns `Err(RecvTimeoutError::Timeout)` on tick and
  `Err(Disconnected)` when the daemon drops — the latter must keep today's
  "connection to the daemon closed" behaviour (`client.rs:341`).
- The stdin thread cannot be cancelled once blocked in `read_line`. Detach it;
  process exit reaps it. Do not attempt a join on the shutdown path.
- `conn.call` (`client.rs:155`) keeps its own pump loop unchanged — a turn in
  flight still drains events the same way. Only the *between-turns* blocking
  point moves.
- Watch the ordering contract: a line typed while an event is in flight must
  not be reordered ahead of it. One channel gives FIFO for free; do not add a
  second queue that would reintroduce the ordering question.

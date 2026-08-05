---
id: TASK-038
title: "Unified input+event channel: the interactive entry loop stops blocking on stdin"
status: complete
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

Per ADR-556-1 (as revised during implementation): keep exactly one reader of
stdin — the main thread — and make the *wait* interruptible with a `poll` on
fd 0. Between polls the loop drains the event channel and renders anything
queued.

**The piped path must not be touched.** Non-TTY sessions keep calling
`FramedStdinPrompter::ask` exactly as today. That is what makes AC-4's
byte-identity true by construction rather than by careful matching.

## Files to Create/Modify

- `crates/teton/src/client.rs` — `Drained`; `Connection::drain_events(ctx, on_first)`, a non-blocking drain that renders through the existing `dispatch_event`
- `crates/teton/src/prompt.rs` — `stdin_ready(timeout)` (a `libc::poll` on fd 0); split `FramedStdinPrompter::ask` into `draw` / `erase` / `read_line` so the entry frame can stay open across a wait and be torn down around a render
- `crates/teton/src/main.rs` — `next_interactive_line`, the poll-and-drain wait; `FRAME_INTERVAL`; the interactive branch of `run_session` routed through it, the non-interactive branch left on `ask`

**Design changed during implementation — see ADR-556-1's superseded draft.** The
task was written against a stdin *reader thread* feeding the connection's
channel. That plan has a defect: `dispatch_event` answers permission and
model-proposal prompts with their own blocking `read_line`, so a reader thread
would be a second reader of stdin and a line typed while a consent prompt was
open would go to whichever reader the kernel woke first. One reader plus an
interruptible *wait* replaces it, and is strictly smaller.

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
- [ ] Unit test: `drain_events` is exercised with **no socket server and no
      terminal** — `UnixStream::pair` supplies the writer half and the test
      feeds the channel directly. Covers arrival-order preservation, `on_first`
      firing exactly once per drain (an open frame is torn down once, not once
      per event), an empty channel touching nothing (no idle flicker), and a
      disconnected channel reading as "nothing queued" rather than an error.

## Technical Notes

- `Connection` holds `incoming: Receiver<Incoming>`, fed by `reader_loop`
  spawned in `connect`. `drain_events` uses `try_recv`, so it never blocks.
- `libc` is already a direct dependency, used exactly this way for `TIOCGWINSZ`
  in `prompt.rs` — `poll` on fd 0 follows the established local pattern and adds
  nothing to the dependency graph.
- `POLLIN` fires for both "bytes available" and "at EOF"; the caller
  distinguishes them by `read_line` yielding zero bytes. A `poll` **error**
  (notably `EINTR`) must read as "not ready", never as EOF — otherwise a stray
  signal ends the session.
- `conn.call` keeps its own pump loop unchanged — a turn in flight still drains
  events the same way. Only the *between-turns* blocking point moves.
- The frame must be torn down before anything renders over it and redrawn
  after, which is why `drain_events` takes an `on_first` hook rather than the
  caller erasing unconditionally: erasing every interval would flicker an idle
  session at the frame rate.
- Ordering is preserved for free — one channel, `try_recv` in arrival order,
  and stdin is only read when poll says it is ready.

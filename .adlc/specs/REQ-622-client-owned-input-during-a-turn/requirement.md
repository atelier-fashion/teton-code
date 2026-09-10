---
id: REQ-622
title: "Client-owned input while a turn is working — raw-mode type-ahead the activity row never overwrites, that never answers a question unseen, and a terminal restored on every exit"
status: draft
deployable: true
created: 2026-09-10
updated: 2026-09-10
component: "cli"
domain: "harness"
stack: ["rust", "cli"]
concerns: ["developer-experience", "reliability", "security"]
tags: ["raw-mode", "termios", "canonical-mode", "echo", "activity-row", "type-ahead", "stdin", "tty", "signals", "pty-testing", "scrollback", "bug-225"]
---

## Description

REQ-621 gave a running turn a live activity row, and shipped one recorded exception:
when the user types a line and presses Enter while the row animates, the row is
abandoned and its last frame stays in scrollback (BUG-225). The exception exists because
the interactive session leaves the terminal in **canonical mode with echo on** during a
turn. The kernel assembles the line, echoes every keystroke at the cursor, and on Enter
drops the cursor a row — none of which the client can observe. A client that repaints
one row above the cursor has therefore lost its geometry the instant a line is
submitted, and the only safe move was to stop painting.

The same mode has a second, older hazard that the row made visible but did not cause:
a line typed during a turn sits in the kernel's buffer, and the next thing that reads
stdin gets it. If that next reader is a **permission prompt** opened mid-turn, the
type-ahead is consumed as the answer to a question the user never saw.

This REQ fixes both at the root: while a turn is running at a terminal, the client owns
its input. It takes the terminal out of canonical mode for the length of the turn,
reads keystrokes itself, echoes and edits the pending line on a row it owns, keeps that
line aside from any question the daemon asks, submits it as the next prompt when the
turn ends, and puts the terminal back exactly as it found it — on every exit, including
the signals that end the process. REQ-556's one-reader-of-stdin rule stays intact:
there is still exactly one reader, it is just no longer the kernel's line discipline.

Why now: the row's BR-5 and BR-9 promises hold only until the user types Enter, and a
user who types during a slow turn is the common case, not the edge. The existing
echo-off key prompt already carries an "accepted residual" that Ctrl-C leaves the
terminal with echo off (REQ-572): a raw window that lasts a whole turn cannot accept
that residual, so restoration on signal exit is part of this REQ and retires that one.

## System Model

_Every entity is client-side and terminal-scoped. Nothing here reaches the daemon or
the wire._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| InputMode | mode | enum | `canonical` (today's behaviour) or `raw`; `raw` only while a turn is running and both stdin and stdout are terminals |
| TerminalGuard | saved | opaque | the terminal settings as they were before `raw` was entered; restored verbatim on every exit path, including signal exit |
| TypeAhead | pending | string | the line being typed, valid UTF-8, edited by Backspace; rendered by the client on a row it owns |
| TypeAhead | queued | list of string | lines submitted with Enter during the turn, in order; each becomes a prompt after the turn ends |
| TypeAhead | shelved | boolean | true while a daemon question owns the input; `pending` is hidden and preserved, and is shown again when the question is answered |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| turn begins at a terminal | `session/prompt` is sent with a live-row surface | mode → `raw`; `TerminalGuard` armed |
| printable keystroke | user types | appended to `pending`, echoed by the client; multi-byte sequences assembled before echo |
| Backspace | user types | last character removed from `pending` and from the echo |
| Enter | user types | `pending` moved to `queued`; shown once as a queued line; `pending` cleared |
| question opens mid-turn | permission request, model proposal, attach consent, over-budget offer | `shelved` = true; the question reads only keystrokes typed after it was drawn |
| question answered | user answers | `shelved` = false; `pending` shown again |
| turn ends by any path | result, RPC error, transport error, daemon disconnect | mode → `canonical`; guard restores; `queued` lines submitted in order as the next prompts |
| Ctrl-C | user types | today's meaning (the session ends); guard restores first; nothing partial is submitted |
| Ctrl-D | user types during a turn | ignored; it has no meaning until a prompt is open (OQ-1) |
| line queued | Enter during a turn | the activity row gains a `· N queued` clause while `queued` is non-empty (OQ-3) |
| process-ending signal | SIGINT, SIGTERM, SIGHUP | guard restores before the process dies |
| raw mode unavailable | `tcsetattr` fails on a real tty | mode stays `canonical`; today's REQ-621 behaviour (abandon on a submitted line) applies; one verbose notice |

## Business Rules

_Explicit, testable constraints governing this feature's behavior._

- [ ] BR-1: **Raw only inside a turn, only at a terminal.** The terminal leaves canonical mode when a turn starts on a live-row surface and returns to it when the turn ends. Between turns the entry prompt reads exactly as it does today. With stdin or stdout not a terminal, nothing changes: piped output and piped input behaviour are byte-identical to today (informed by REQ-621 BR-6, REQ-556 BR-2).
- [ ] BR-2: **One reader of stdin, still.** The client is the only reader; every question a turn can open (permission, model proposal, attach consent, over-budget offer) obtains its answer through the same reader. No second thread or second descriptor reads the terminal (informed by REQ-556 ADR-556-1).
- [ ] BR-3: **What the user types is visible, editable, and never overwritten.** Printable characters (assembled from UTF-8, including CJK and emoji), Backspace, and Enter are handled. The pending line is echoed by the client on a row it owns; no activity-row repaint, withdraw, or durable line ever lands on it. REQ-621 BR-9 becomes unconditional.
- [ ] BR-4: **A submitted line leaves no frame behind.** The activity row is erased cleanly on every turn exit whether or not the user typed. After the turn, scrollback equals today's scrollback plus each queued line shown exactly once where the next prompt echoes it. REQ-621 BR-5's recorded exception is retired (informed by BUG-225, LESSON-659).
- [ ] BR-5: **Type-ahead is never an answer.** A question opened mid-turn reads only keystrokes typed after it was drawn. Anything pending at that moment is shelved, preserved verbatim, and shown again once the question is answered. A queued line is never consumed by a question either.
- [ ] BR-6: **Enter queues the next prompt.** Each line submitted during the turn is queued in order and, after the turn ends, submitted as the next prompt exactly as if typed at the entry frame — including slash commands, the REQ-615 `cd` intercept, and every pre-send check the entry path already runs. Nothing is sent to the daemon while the turn is still running. A multi-line paste arrives as several Enters and queues one prompt per line; bracketed paste is a later REQ (OQ-2).
- [ ] BR-7: **The terminal is restored on every exit.** Normal turn end, RPC error, transport error, daemon disconnect, a client panic, and the signals that end the process (SIGINT from Ctrl-C, SIGTERM, SIGHUP) all put the terminal back exactly as it was. The same restoration covers the existing echo-off key prompt, retiring REQ-572's accepted residual. The mechanism adds no second stdin reader and no timer thread.
- [ ] BR-8: **Ctrl-C keeps its meaning.** It ends the session as it does today; the terminal is restored first and no partial line is submitted.
- [ ] BR-9: **Unhandled control input is ignored, never echoed, never forwarded.** An escape-prefixed sequence (arrow keys, function keys) is consumed as a unit and dropped; a lone control byte outside the handled set is dropped. Nothing typed can reach the daemon except through BR-6.
- [ ] BR-10: **Pure content, gated bytes.** The line editor's state and its rendering are pure functions of the keystrokes seen, unit-tested with no terminal; only the terminal-mode calls and the bytes written are gated (informed by LESSON-481, REQ-560 BR-8).
- [ ] BR-11: **Failing to enter raw mode fails open to today's behaviour, and says so.** If a real terminal refuses the mode change, the turn runs in canonical mode with REQ-621's abandon-on-submit behaviour, and one verbose notice names it. This is the opposite polarity from the echo-off prompt, which fails closed because it hides a secret; here there is nothing to hide (informed by REQ-572).
- [ ] BR-12: **No wire change.** No method or event changes; `PROTOCOL_VERSION` is unchanged.
- [ ] BR-13: **One owner of the terminal during a turn.** The row and the type-ahead echo are painted and withdrawn by the same loop that holds the clock, under REQ-621's withdraw-before-durable-write discipline; every other writer sees both withdrawn (informed by REQ-621 ADR-621-3).
- [ ] BR-14: **The row says a line is queued.** While `queued` is non-empty the activity row carries a `· N queued` clause with the exact count, so an Enter that registered is visible without waiting for the turn to end; the clause is withdrawn with the row and is subject to REQ-621's fit rule.
- [ ] BR-15: **Ctrl-D is inert during a turn.** It neither ends the session nor submits anything; EOF keeps its meaning only at an open prompt, as today.

## Acceptance Criteria

- [ ] AC-1: **A submitted line is not overwritten and leaves no residue.** PTY e2e: type a line and press Enter while the row animates during a delayed turn; the echoed line is intact on the rendered screen at every later frame, the row keeps animating beneath it, and after the turn the replayed screen shows no activity glyph and the line appears exactly once as the next prompt's text.
- [ ] AC-2: **Type-ahead becomes the next prompt.** PTY e2e: two lines submitted during one turn become the next two prompts in order, each producing its own reply; a queued `/cost` runs as the slash command.
- [ ] AC-3: **A question never eats type-ahead.** PTY e2e: with a pending partial line, a tool that needs permission opens its prompt; the prompt is answered only by the key typed after it appeared; the partial line is shown again afterwards and its later submission reaches the daemon intact.
- [ ] AC-4: **Ctrl-C restores the terminal.** PTY e2e: Ctrl-C mid-turn ends the session, and the pty's terminal settings read back by a child process on the same pty afterwards equal the settings captured before the session started (canonical and echo flags identical).
- [ ] AC-5: **Every exit restores.** PTY e2e for a normal end, an RPC error, the daemon killed mid-turn, SIGTERM to the client, and a client panic provoked through a debug-only seam: each leaves the terminal settings equal to the pre-session capture.
- [ ] AC-6: **The key prompt's residual is gone.** PTY e2e: Ctrl-C at the echo-off provider-key prompt leaves echo on afterwards.
- [ ] AC-7: **Piped is untouched.** Every existing pipe fixture passes byte-for-byte; a piped-stdin session with a terminal stdout never enters raw mode (no termios call is made) and behaves as today.
- [ ] AC-8: **Multi-byte input round-trips.** PTY e2e: `é`, CJK, and an emoji typed mid-turn are echoed once each, a Backspace removes one whole character, and the submitted line reaches the daemon byte-identical.
- [ ] AC-9: **Unhandled keys are inert.** PTY e2e: arrow keys and a function key typed mid-turn echo nothing and change nothing; the next submitted line is exactly what was typed.
- [ ] AC-10: **The editor is pure.** A unit table maps keystroke sequences to `(pending, queued, echo bytes)` with literal oracles; the mutation "Backspace removes a byte, not a char" is applied and observed red, and recorded (informed by LESSON-569).
- [ ] AC-11: **Raw-mode refusal falls back honestly.** A unit test with a terminal double that refuses `tcsetattr` shows the turn proceeds, the REQ-621 abandon path is used, and exactly one verbose notice is printed.
- [ ] AC-14: **A queued line is announced on the row.** PTY e2e: after Enter mid-turn the next frame carries `· 1 queued`; a second Enter makes it `· 2 queued`; the clause is gone after the turn.
- [ ] AC-15: **Ctrl-D mid-turn is inert.** PTY e2e: Ctrl-D during a delayed turn ends nothing and submits nothing; the turn completes and the entry prompt returns; Ctrl-D at that prompt still ends the session.
- [ ] AC-16: **A pasted block queues one prompt per line.** PTY e2e: three lines written to the pty in one write during a turn become three queued prompts in order.
- [ ] AC-12: **Honest legs.** Every PTY leg runs under the harness's binary-freshness guard and every TTY claim above has a real PTY test (informed by BUG-164, BUG-191, LESSON-510).
- [ ] AC-13: **Closed out.** BUG-225 is marked resolved naming this REQ; REQ-621's BR-5 and BR-9 amendments are marked retired with a dated note; the README describes typing during a turn and the queued-prompt behaviour.

## External Dependencies

- None required by the requirement. The `libc` crate is already a dependency of the CLI; whether a signal-handling crate is used is an architecture decision.

## Assumptions

- Restoring terminal settings from a signal handler is possible with async-signal-safe calls only, so a handler that restores and re-raises needs no allocation and no lock.
- The daemon does not need to know a line was queued; queued prompts are ordinary prompts sent after the turn, so REQ-580's held-turn and REQ-586's budget rules apply unchanged.
- The entry-frame prompter between turns can stay in canonical mode; a full line editor at the prompt is a separate REQ.
- Windows is out of the charter's scope, so termios is the only terminal API this REQ must handle.

## Open Questions

- [x] OQ-1: Ctrl-D during a turn — **resolved 2026-09-10: ignored** (BR-15, AC-15).
- [x] OQ-2: Multi-line paste — **resolved 2026-09-10: one queued prompt per line now**; bracketed paste stays out of scope (BR-6, AC-16).
- [x] OQ-3: Queued count on the row — **resolved 2026-09-10: yes**, a `· N queued` clause (BR-14, AC-14).

## Out of Scope

- A full line editor at the entry prompt (history, arrow-key cursor movement, completion); raw mode between turns.
- Bracketed paste and mouse input.
- Cancelling a turn from the keyboard (no cancel method exists; separate REQ).
- Any daemon or protocol change.
- Windows console modes.

## Retrieved Context

- LESSON-659 (lesson, score 14): Measure a terminal row after its last transform, as a string — and a submitted line is a cursor move the client cannot see
- REQ-621 (spec, score 14): A live activity line while a turn is working
- BUG-164 (bug, score 11): A targeted e2e run can pass against a stale daemon binary
- BUG-189 (bug, score 11): Two refusal reasons publish no record, so the session surface never says why
- BUG-191 (bug, score 11): AC-6 and AC-14 claim a pty leg for the acknowledgment prompt bytes; the pty suite has none
- LESSON-510 (lesson, score 11): A harness that checked a binary exists has not checked it is the one under test
- REQ-560 (spec, score 11): Named permission levels and the interactive session status line
- LESSON-568 (lesson, score 9): An ADR's causal sentence is exactly as unverified as an untested line of code
- LESSON-569 (lesson, score 9): Seven assertions that passed but could not fail — and the three ways they got that way
- LESSON-548 (lesson, score 9): A refusal's remedy is a claim about the product's own surface
- BUG-173 (bug, score 9): The pty suite's entry-prompt wait absorbs daemon startup, so a slow CI runner reads as a failing test
- LESSON-481 (lesson, score 9): A gate that hides a feature from users also hides it from the test suite
- REQ-615 (spec, score 8): Session-root honesty for the shell tool and skill preambles
- REQ-617 (spec, score 8): The model knows the session's own commands and stops repeating itself
- LESSON-628 (lesson, score 8): Announce on what was rendered, not on what was stored

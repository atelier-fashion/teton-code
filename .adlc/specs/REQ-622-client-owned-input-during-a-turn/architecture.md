# REQ-622 — Architecture

## Approach

REQ-621 already put the client's event pump on a 120 ms wake and made it the sole
owner of an in-place terminal row. REQ-622 extends that same loop in three
directions and adds nothing outside it:

1. **The pump reads keystrokes.** With the terminal out of canonical mode, the tick
   that already asks `stdin_ready(Duration::ZERO)` now *reads* the bytes that are
   waiting (one non-blocking `read(2)` after the `poll`) and feeds them to a pure
   line editor. The main thread stays the single reader of stdin (ADR-556-1); the
   kernel's line discipline is simply no longer in front of it.
2. **The pump owns two rows.** Beneath the activity row it draws the pending line
   the user is typing. Both rows are withdrawn before any durable write and redrawn
   after, under the discipline ADR-621-3 established for one row.
3. **A terminal guard that survives the process.** A `RawMode` guard mirrors the
   REQ-572 `EchoOff` guard (save, change, restore on `Drop`) and registers its
   saved settings in one process-wide restore slot that a `sigaction` handler
   replays before re-raising the signal. `EchoOff` registers in the same slot, so
   REQ-572's Ctrl-C residual closes for free.

Everything else is plumbing on existing seams: a question mid-turn reads its
answer through the same editor with the pending line shelved; Enter moves the
pending line to a queue on `SessionState` that the entry loop drains ahead of its
poll, so a queued line takes exactly the typed-line path. No wire change.

## Key decisions

### ADR-622-1: Only `ICANON` and `ECHO` change, only inside a turn, only at a terminal

**Decision.** `RawMode::engage` clears `ICANON` and `ECHO` and sets `VMIN=0`,
`VTIME=0`; it leaves `ISIG`, `ICRNL`, `OPOST`, and every other flag as found.
It is engaged by `Connection::call` at the start of an `ENDS_TURN` method when
`ctx.surface.has_live_rows()` **and** `ctx.typed_input` are both true, and
dropped at that call's close-out. `engage` uses `TCSAFLUSH`; `Drop` uses
`TCSANOW`, with `EchoOff`'s reasons.

**Rationale.** Keeping `ISIG` means Ctrl-C is still a `SIGINT`, so BR-8's "today's
meaning" holds by construction and the restore path is the signal path
(ADR-622-3). Keeping `ICRNL` and `OPOST` means Enter still arrives as `\n` and
every `line()`/`fragment()` the surface writes still ends a row the way it does
today — clearing `OPOST` would make every existing byte assertion in the pty
suite false. The two-flag change is the smallest that makes the client the
reader; `EchoOff`'s one-flag change is the precedent for touching nothing else.
Gating on both `has_live_rows` (stdout) and `typed_input` (stdin) is BR-1: a
piped stdin with a terminal stdout never leaves canonical mode and never calls
`tcsetattr` (AC-7).

**Consequences.** Between turns the entry prompt reads exactly as today
(canonical, `read_line`). A `tcsetattr` failure on a real tty leaves canonical
mode in place, keeps REQ-621's `RowState::abandon` path armed, and prints one
verbose notice — BR-11's fail-open, the opposite polarity from `EchoOff` and
documented as such at the classifier.

### ADR-622-2: A pure line editor that both the pump and the prompter feed

**Decision.** New `crates/teton/src/input_editor.rs`: `InputEditor` holds
`pending: String`, `queued: Vec<String>`, `shelved: Option<String>`, and a
partial UTF-8 accumulator; `push(&mut self, bytes: &[u8]) -> Vec<Edit>` decodes
keystrokes — printable UTF-8 characters, Backspace (`0x7f`/`0x08`), Enter
(`\n`/`\r`), `ESC [`/`ESC O` sequences consumed as a unit and dropped, every
other control byte dropped (Ctrl-D included, BR-15) — and returns what changed;
`row(&self, width) -> Option<String>` renders the pending line as one row (the
tail that fits, prefixed by a fixed marker); `shelve()`/`unshelve()` move
`pending` aside for a question and back. `take_queued()` drains the queue.

**Rationale.** BR-10 / LESSON-481: the editor is the content, the pump and the
prompter are the gated bytes. One editor serves two callers so BR-2 has one
reader and BR-5 has one seam (LESSON-502): a question's answer is read by
`FramedStdinPrompter::ask` through a *fresh* editor buffer while the pump's
`pending` sits in `shelved`; no path reads stdin any other way while raw mode is
engaged. The prompter learns it is in raw mode from `RawMode::is_engaged()`, a
property of the terminal (process-global), not a flag threaded through the 19
`UiContext` construction sites.

**Consequences.** `SessionState` gains `input: InputEditor`; `begin_turn` does not
clear it (queued lines must survive into the next turn). The AC-10 oracle is a
table of byte sequences to literal `(pending, queued, echo)` triples.

**Amendment, 2026-09-10 (verify).** "One editor serves two callers" above reads as
though one `InputEditor` *value* is shared; it is not, and the implementation was
never that. There is **one `InputEditor` type and two independent instances**: the
pump's, which lives on `SessionState` and holds the line being typed into the turn
plus the queue, and each prompter's, a fresh answer buffer created for the life of
one question (`FramedStdinPrompter::answer`, `StdinPrompter::answer`, reset at the
top of every `read_answer_raw`). What is shared is the **decoding** — one
implementation of "which bytes are a character, a Backspace, an Enter, an escape
sequence to drop" — which is what BR-2's single reader and BR-10's purity are
actually about. `shelve`/`unshelve` are a hand-off **on the pump's own copy** and
move nothing between the two instances: `shelve` puts the pump's pending line
aside so the pump's buffer is empty across the question, `unshelve` puts it back
verbatim, and whatever the prompter's own buffer holds is discarded rather than
merged. There is no shared storage anywhere on the path, which is why a question
cannot be answered by the sentence it interrupted even by accident.

### ADR-622-3: One restore slot, a `sigaction` handler that restores and re-raises

**Decision.** `prompt.rs` gains a process-wide restore slot: a `static` holding an
armed flag and a copy of the saved `termios`, written by `RawMode::engage` and
`EchoOff::engage` and cleared by their `Drop`. A handler installed with
`libc::sigaction` for `SIGINT`, `SIGTERM`, and `SIGHUP` does exactly three
async-signal-safe things: if the slot is armed, `tcsetattr(STDIN_FILENO,
TCSANOW, &saved)`; reset that signal's disposition to `SIG_DFL`; `raise` it.
Handlers are installed once, lazily, the first time a guard engages.

**Rationale.** BR-7 names the signals; `tcsetattr`, `sigaction`, and `raise` are on
POSIX's async-signal-safe list, so the handler needs no allocation and no lock.
Re-raising after restoring keeps every exit status and every parent-visible
semantics the kernel gives today (Ctrl-C still ends the session with the SIGINT
status, BR-8). This is preferred over a polled signal crate: the CLI's manifest
treats its thin dependency set as a property, a polled design cannot restore
while the main thread is inside a blocking call, and the handler is ~40 lines of
`unsafe` scoped to the terminal seam that already holds every other
`libc` call. `EchoOff` registering in the same slot is what retires REQ-572's
accepted residual (AC-6) without a second mechanism.

**Consequences.** A panic unwinds through `Drop` (no `panic = "abort"` in any
profile — verified), so AC-5's panic leg needs no handler. A debug-only seam
(`TETON_TEST_SEAMS=1` plus `TETON_TEST_PANIC_MID_TURN=1`) panics the pump after
its first tick so the pty leg can provoke it. The slot is a single global on
purpose: two guards cannot be engaged at once (a question inside a turn reuses
the turn's raw mode; `EchoOff` is only reachable between turns).

### ADR-622-4: Two rows under one owner; Enter queues, it never prints

**Decision.** `RowState` becomes a two-row block: the activity row (as today)
and, beneath it, the editor's pending row. `paint_rows` draws whichever exist,
`withdraw_rows` removes both, and the withdraw-before-dispatch rule covers the
block. The cursor rests at the end of the pending row when there is one. On
Enter the pending line moves to `queued`, the pending row is withdrawn, and the
activity row gains the `· N queued` clause (BR-14, a clause on the existing
frame, subject to REQ-621's fit rule). Nothing durable is printed for a queued
line during the turn.

**Rationale.** BR-4: the only place a queued line is ever printed is where the
next prompt echoes it, so scrollback after the turn is today's plus that echo.
BR-13: one owner. BR-3: the pending row is the client's, so no repaint can land
on the user's text — the geometry problem BUG-225 describes cannot arise because
the kernel no longer moves the cursor.

**Consequences.** `RowState::abandon` and `line_waiting` stay for the BR-11
fallback path only; their tests move to the fallback fixture. `repaint_row_above`
offsets become 1 or 2 depending on which rows exist — pinned by the block, not
by arithmetic at call sites.

**Amendment, 2026-09-10 (verify).** Two corrections, both about what the block's
verbs do rather than about the decision above. First, **the block's rows hold what
the renderer holds**: they are drawn with `Surface::draw_row` and repainted and
withdrawn without emitting held text (commit `83235fd`), because the block is
redrawn after every streamed token and a draw that flushed ended the streamed line
at each one — a reply typed past came out one token per row. The held line goes
out where the block was, on the next durable write or the turn's `end_block`.
Second, **the pending row is the *current* row and not a row above the cursor**:
it is drawn with no trailing newline (`Surface::draw_current_row`), repainted in
place with `\r` + erase (`repaint_current_row`) and cleared where the cursor
already stands (`withdraw_current_row`), so the caret rests at the end of the
user's own text as this ADR says it does. The consequence for the offsets is that
they collapse: the activity row is **always exactly one** above the cursor, with a
pending row beneath it and without, so `1 or 2` becomes `1` and the block's only
arithmetic disappears.

### ADR-622-5: Queued lines re-enter at the poll, not at the dispatcher

**Decision.** `next_interactive_line` checks `state.input.take_next_queued()`
before it polls stdin. A queued line is returned exactly as a typed line would
be, after the entry frame has been drawn with the line echoed in its input row
(so the user sees what is about to be sent, once — BR-4), and then flows through
`slash::classify` and every pre-send check unchanged (BR-6).

**Rationale.** Re-entering above the classifier means the REQ-615 `cd` intercept,
skill invocation, the CLI mirror, and the REQ-581 turn record all apply without
a second code path (LESSON-502's "collapse the seams"). Draining one line per
loop iteration, not all at once, keeps each queued prompt a separate turn with
its own hand-off, cost line, and — if it opens a question — its own prompt.

**Consequences.** A queued line is never sent while a turn is running (BR-6);
`/quit`-style commands queued mid-turn take effect in order after the turn.

### Decisions recorded against the spec's open questions

All three were resolved by the user before architecture. OQ-1 → ADR-622-2
(Ctrl-D dropped by the editor); OQ-2 → ADR-622-2 (each `\n` in a pasted block is
an Enter); OQ-3 → ADR-622-4 (the queued clause).

## Files affected

| File | Change |
|---|---|
| `crates/teton/src/input_editor.rs` | **new** — `InputEditor`, keystroke decoding, `row`, shelve/unshelve, queue; tests and mutation record |
| `crates/teton/src/prompt.rs` | `RawMode` guard; the restore slot and `sigaction` handler; `EchoOff` registers in the slot; `FramedStdinPrompter::ask` raw read path through the editor; `read_available` (poll + read) |
| `crates/teton/src/client.rs` | engage/drop `RawMode` in `call` for `ENDS_TURN` at a TTY; tick reads bytes into the editor; two-row `RowState`; shelve/unshelve around questions; the panic seam |
| `crates/teton/src/session_ui.rs` | `SessionState::input: InputEditor` |
| `crates/teton/src/activity.rs` | `· N queued` clause |
| `crates/teton/src/main.rs` | queued-line drain in `next_interactive_line`; frame echo of a queued line |
| `crates/teton/tests/pty_e2e.rs`, `cli_e2e.rs`, `common/mod.rs` | AC-1..9, 12..16 legs; harness: signal delivery, `stty -a` readback, control bytes |
| `.adlc/specs/REQ-621-*`, `.adlc/bugs/BUG-225-*`, `README.md`, `CHANGELOG.md`, `docs/manual-verification.md`, `.adlc/context/architecture.md` | AC-16 close-out |

## Proposed additions to `.adlc/context/architecture.md`

- Under Key Patterns, beside "A live row is owned by the pump that can wake":
  **A terminal mode change registers its undo where a signal can find it** — every
  guard that alters termios saves into one process-wide slot, and one handler
  restores from that slot and re-raises; a guard whose undo lives only in `Drop`
  is a guard that lies about Ctrl-C (REQ-622 ADR-622-3, retiring REQ-572's
  residual).

## Lessons applied

- **LESSON-481** — the editor is pure; only termios calls and bytes are gated.
- **LESSON-502** — one seam for "type-ahead never answers" and one restore slot for every mode change; an adversarial test at each caller of `ask`.
- **LESSON-659** — the row's geometry is owned, never inferred from a kernel the client cannot see.
- **LESSON-568** — every causal sentence above has a named mutation in the task that lands it.
- **LESSON-510 / BUG-164 / BUG-191** — every TTY claim has a pty leg under the freshness guard.

# REQ-621 — Architecture

## Approach

Everything the row needs is already on the wire and already flows through one
place: `Connection::pump_until_answered` (`crates/teton/src/client.rs`) reads
every frame the daemon sends during a `session/prompt` call and hands each
event to `session_ui::render_event`. The requirement is therefore two
problems, and the first is transport-shaped exactly as REQ-556's was:

1. **The pump cannot wake without the daemon.** `Connection::recv` is a plain
   `mpsc::Receiver::recv()` with no timeout (`client.rs:800`), so during a
   silent stretch nothing runs on the client at all. REQ-556 solved the same
   shape for the entry loop by making the *wait* interruptible rather than the
   read (ADR-556-1: `poll` on stdin with `FRAME_INTERVAL`). Here the channel is
   a `std::sync::mpsc` receiver fed by the reader thread, and
   `Receiver::recv_timeout` is in the standard library. The wake is one call.
2. **Once the pump ticks, the row is a projection.** Every phase in the spec's
   entity table is a function of events the daemon already publishes plus the
   client's own clock. A pure state machine in a new module, folded by the pump
   and rendered through the existing `Surface` seam, is the REQ-556 shape again
   (`loading.rs`), and it inherits that module's verification route: the frame
   is assertable from a plain unit test with the TTY gate out of the way.

No wire change. `PROTOCOL_VERSION` stays 2 (`teton-protocol/src/lib.rs`).
BR-14's room for an additive event is not used: after a tool result the
phase is `awaiting_model` by construction (the next thing the daemon can send is
a chunk, a tool call, or the result), and `route_decided` already carries the
provider, tier, and optional model.

## Key decisions

### ADR-621-1: The wake is a timed receive on the existing channel, at the TTY only

**Decision.** `Connection` gains `recv_timeout(Duration)` returning
`Wake::Message(Incoming) | Wake::Tick`. `pump_until_answered` uses it **only
when the surface has live rows** (ADR-621-3); otherwise it keeps the blocking
`recv()` it has today. On `Tick` the pump stamps the clock into the activity
state, computes a frame, and repaints; on `Message` it withdraws the row,
dispatches as before, folds the event, and redraws.

**Rationale.** `std::sync::mpsc::Receiver::recv_timeout` exists; no crate, no
second thread, no change to `Incoming` or the reader. Gating the timed receive
on live rows is what makes AC-3 hold *by construction*: the piped path keeps
the blocking call and never enters the tick arm, so it cannot emit a byte it
did not emit before — the same mechanical expression of BR-6 that ADR-556-1
chose for BR-2. The tick interval reuses `FRAME_INTERVAL` (120 ms), moved from
`main.rs` to a shared constant so the two animations share one clock.

**Consequences.**
- The stall clock, the elapsed counters, and the spinner all advance from this
  one tick. There is no timer thread and no `sleep`.
- `recv_timeout` returning `Disconnected` is the transport error the pump
  already maps to "connection to the daemon closed" — BR-12's disconnect path
  falls out of the existing error return (see ADR-621-4).
- A `Tick` that finds the activity idle (a non-turn RPC such as `/cost` pumping
  through the same loop) paints nothing. The projection, not the method, decides
  whether there is a row.

### ADR-621-2: The activity is a pure projection with the clock passed in

**Decision.** New `crates/teton/src/activity.rs` holds `TurnActivity`:

| Field | Set by |
|---|---|
| `phase: Phase` (`Idle`, `Preparing`, `AwaitingModel`, `Streaming`, `ToolRunning`, `AwaitingPermission`, `Held`, `Compacting`) | `observe` |
| `detail: Option<String>` | `observe` — provider/tier/model from `route_decided`, the daemon's title from `tool_call`, the daemon's sentence from `turn_queued` |
| `turn_started`, `phase_since`, `last_event: Instant` | `begin(now)`, `observe(.., now)` |
| `cost_micros: i64` | `observe(cost_recorded)` — exact sum, this turn only |
| `model_time`, `tool_time: Duration` | accrued on each phase exit, read by the BR-16 summary |
| `resume_after_permission: Option<Phase>` | `observe(permission_request)`; restored by `permission_answered(now)` |

Three methods carry the contract: `observe(&EventEnvelope, own_session, now)`
folds one event (ignoring other sessions per BR-15, reading `session_id` the
way `other_session` in `session_ui` does); `frame(now, tick) -> Option<Frame>`
is the pure render — no I/O, no terminal, and **no clock of its own**: `now`
is a parameter, so a test hands it synthetic instants; `finish(now) ->
TurnSummary` closes the turn and yields the BR-16 figures.

`frame` returns `None` for `Idle`, `Streaming`, and `AwaitingPermission`
(BR-1), and for `Streaming` only until the stall bound is crossed (BR-11, OQ-2).
The text is `<spinner> <phase sentence> · <phase elapsed>s · turn <turn elapsed>s[ · $cost]`,
truncated to the surface width via the same `unicode-width` measurement
`markdown.rs` uses (ASSUME-023), with the cost formatted by
`cost_ui::format_usd` so the row and the meter never disagree.

**Rationale.** BR-7 and LESSON-481: the TTY gate hides the row from every
piped test, so if the frame lived in the render path it would have no
verification route. Passing `now` rather than reading it keeps the function
pure while letting elapsed time be *measured* (BR-3) — the measurement happens
where the clock is, in the pump, and the frame only formats a duration it was
given. LESSON-569: the unit oracle is a table of literal expected strings, not
a call back into `frame`.

**Consequences.** BR-3's prohibitions are structural: `frame` has nothing to
derive an ETA from, and the only cost it can print is the integer it was given.
`SessionState` gains one field, `activity: TurnActivity`, beside `loading` and
`cost`, and `begin_turn` arms it (`Preparing`, all instants = `now`).

### ADR-621-3: One row, owned by the pump, withdrawn before anything else writes

**Decision.** The row is drawn with `Surface::line(LineKind::Activity, ..)`
the first time a frame appears, repainted in place with the existing
`repaint_row_above(1, ..)` on later ticks, and removed with a **new** verb
`Surface::withdraw_row_above(rows_up)` (`\x1b[{n}A\r\x1b[K`, cursor left at the
start of the cleared row). The pump is the row's only owner and follows one
discipline: **withdraw before dispatching any message, redraw after**. A
durable line (`shell: … [running]`, a notice, a permission prompt) therefore
always prints where the row was, and the row reappears beneath it.

`Surface` also gains `has_live_rows(&self) -> bool`, default `false`, `true`
only for the `PlainSurface::with_markdown` constructor — the one chosen at the
CLI edge for an interactive terminal (`main.rs:1161`). That is the TTY gate the
pump reads (ADR-621-1); `withdraw_row_above` defaults to a no-op like
`repaint_row_above`, so every non-terminal surface, including future ones, is
silent by construction (BR-6, BR-8).

**Rationale.** Answers BR-5 directly: a withdrawn row is not in scrollback.
The alternative — repainting the row with an empty string — leaves a blank row
behind, which is residue AC-7 would catch. Drawing via `line()` rather than
raw bytes keeps the defuser and the markdown flush ordering (`emit_pending`
before cursor motion, REQ-592 BR-8) in front of the row, and `LineKind::Activity`
gives the styling table one place to dim it (REQ-573: styling is authored in
the sanitizer, never by the caller).

**Consequences.**
- Drawing the row emits any held markdown line first — the same trade
  `line()` makes at every mid-turn interruption (REQ-592). The row is withdrawn
  during `Streaming`, so this only happens when the stream has already paused.
- `RecordingSurface` records `Withdraw(rows_up)` and answers `has_live_rows`
  from a constructor flag, so the wiring is assertable without a terminal;
  `Bare` keeps the defaults and pins them (the existing
  `a_surface_that_does_not_override_repaint_emits_nothing` gains a sibling).
- Typing during a turn — **corrected 2026-09-10 at verify**, where the original
  bullet was found to describe only half of it. The terminal echoes typed
  characters into the row *below* the activity row, which is where the cursor
  sits after `line()` drew it. A repaint saves and restores the cursor, so a
  line typed and **not** submitted is only visually displaced while the row
  animates and reappears when the line is read — the kernel's line buffer is
  untouched (AC-10 asserts delivery, which is the property BR-9 states).
  A line **submitted** mid-turn is the case the bullet missed: `ECHO` is on for
  the length of a turn, so pressing Enter echoes a newline and the cursor drops
  a line under bookkeeping a canonical-mode client cannot see. The row is now
  *two* above the cursor, `repaint_row_above(1)` would rewrite the line holding
  what the user typed and `withdraw_row_above(1)` would erase it, and no offset
  correction is available (the client cannot know how many rows the echo
  wrapped onto). So the pump **abandons** the row for the rest of the turn the
  moment `prompt::stdin_ready(ZERO)` reports a line waiting and `typed_input`
  says stdin is a terminal: no repaint, no withdraw, and that last frame is
  left in scrollback — the bounded BR-5 exception now recorded in the spec, and
  the only option here that does not damage something the user typed.

  **Retired 2026-09-10 by REQ-622 (ADR-622-1, ADR-622-4).** Every sentence in
  this bullet from "`ECHO` is on for the length of a turn" onward is conditional
  on the client leaving the terminal in canonical mode, and REQ-622 stops doing
  that: inside a turn the client holds the terminal in raw mode, echoes the
  pending line itself on a second row it owns beneath the activity row, and
  therefore knows exactly where the cursor is. There is no unobservable
  bookkeeping left to lose the geometry to, so `RowState::abandon` is gone, the
  row animates past a submitted line, and it is withdrawn cleanly at every turn
  exit. The bullet's diagnosis stands and is why REQ-622 exists — it is the
  mitigation that is retired, not the analysis. BUG-225 is resolved on the same
  date.

### ADR-621-4: Every turn exit closes the row at the `ENDS_TURN` seam

**Decision.** `Connection::call` already captures the pump's outcome and
runs `end_block()` on the `P::ENDS_TURN` branch for every return path — Ok,
RPC error, and every transport `?` inside the pump (REQ-592 ADR-3's reasoning,
verbatim). The row's close-out is added to that same branch: withdraw if
visible, then `state.activity.finish(now)`, storing the `TurnSummary` on the
state for the caller. The turn arm in `main.rs` prints the BR-16 line from
that summary on both its Ok and Err arms when `state.verbose` is set.

**Rationale.** Encoding the rule on the method property (`ENDS_TURN`) rather
than on the thirty call sites is the LESSON-568 shape this file already
carries: one site, region-checked. It is also why BR-12 needs no daemon
cooperation — the seam runs on the client's own control flow, including
"connection to the daemon closed".

**Consequences.** The summary line is emitted whether or not stdout is a
terminal (BR-16), so the verbose piped fixtures gain exactly one line
(AC-3, AC-13). Non-verbose piped output is untouched.

**Amended 2026-09-10 at verify.** The close-out landed as **two parts in two
places**, and this ADR described one block on the branch. The implementation is
right and the ADR was not:

- the **withdraw** is hoisted *above* `if P::ENDS_TURN` and guarded by
  `row.visible` and nothing else. That guard is already exact — only a turn can
  open the projection, so only a turn can have a row to take back — and it means
  BR-12 does not rest on the argument that no non-turn call can be in flight
  during a turn. That argument is true today by this client's synchrony, and it
  is precisely the kind of reachability claim BR-12 exists not to depend on;
- the **summary** stays on the `ENDS_TURN` branch, because it must ask: a
  `/cost` that called `finish` would report a turn nobody ran and overwrite the
  last real one.

There is a **second, defensive close-out** at `SessionState::begin_turn`: a turn
still found open when the next prompt goes on the wire ended by a path neither
site saw, and it is closed there rather than overwritten, so its cost and its
clock are not lent to the turn about to start (ADR-621-2). It runs on no
ordinary path.

Both are pinned by source region rather than by argument —
`client.rs::tests::the_rows_close_out_straddles_the_ends_turn_branch`, the same
mechanism `only_the_event_pump_declares_a_block_over` uses on the fence. The
mutation that motivated it: moving the withdraw onto the branch reddens that
one test and **nothing else in 828**, because every behavioural test drives
either a turn (where the branch is taken and the withdraw still runs) or a
non-turn call that never draws a row.

### ADR-621-5: A stall keeps the last phase and annotates it; a running tool is exempt

**Decision.** Past the quiet bound (`STALL_AFTER = 15 s` since `last_event`)
the frame stops the spinner and appends ` · no word from the daemon for <n>s`
to the sentence for the phase the daemon **last reported**; it does not
replace the phase. `ToolRunning` is exempt: the daemon publishes nothing while
a tool runs, so silence there is the expected state and the tool's own elapsed
counter is the honest signal.

**Rationale.** BR-11 as first written turned the phase into `stalled`, which
would have relabelled a 40-second test suite as a stall at 15 s — the noise
that makes a real one easy to miss (REQ-616's own words about the prefill
bar). Keeping the phase and adding the fact is the LESSON-628 posture:
announce on what was rendered, never on a classification the client invented.
The spec is amended to this reading (BR-11, dated 2026-09-10) and validated
again at Phase 3.

**Consequences.** Mid-stream silence (OQ-2) brings the row back in
`Streaming` with the annotation and withdraws it when bytes resume. A hung
tool shows as `running shell: … · 600s`, which is visible for what it is.

### ADR-621-6: The timing legs use a debug-only script directive, not a production delay

**Decision.** `ScriptedFileEngine` (`tetond/src/runtime/mod.rs`) honours a
first line `@delay-ms <n>` in a reply block, honoured only under
`TETON_TEST_SEAMS=1` in a debug build — the gate every other test seam in
`engine.rs` already sits behind. The engine sleeps before emitting the block's
first token. AC-2's running tool is a real tool: a scripted
`{"tool": "shell", "arguments": {"command": "sleep 3"}}` block, no seam.

**Rationale.** REQ-556 left its dots-advancing pty leg uncovered because
"inventing a production-code delay to make a test possible would be the wrong
trade" (`pty_e2e.rs` header). This is not that: the scripted engine is a test
fixture that ships behind a gate a release build refuses, and the directive is
part of the script grammar the fixture already parses. Without it AC-1, AC-6,
and AC-7's kill-mid-turn leg have no deterministic way to hold a turn open, and
BUG-191 is what claiming PTY coverage without the leg looks like.

**Consequences.** AC-6's stall leg costs ~17 s of wall clock, once, in the
pty suite whose window is 60 s. The directive is stripped from the block before
it is streamed, so no test-visible byte changes.

### Decisions recorded against the spec's open questions

All four were resolved by the user before architecture (spec `## Open
Questions`). ADR-621-2 carries OQ-1 (cost so far, exact), ADR-621-5 carries
OQ-2 and OQ-3, ADR-621-4 carries OQ-4.

## Files affected

| File | Change |
|---|---|
| `crates/teton/src/activity.rs` | **new** — `TurnActivity`, `Phase`, `Frame`, `TurnSummary`; `observe` / `frame` / `finish`; unit tests and mutation record |
| `crates/teton/src/render.rs` | `LineKind::Activity`; `Surface::withdraw_row_above` and `has_live_rows` (defaults); `PlainSurface` impls, `live_rows` set by `with_markdown`; `RecordingSurface` records both |
| `crates/teton/src/client.rs` | `FRAME_INTERVAL` home; `Connection::recv_timeout`; the pump's tick arm and withdraw/dispatch/redraw discipline; permission restore; `ENDS_TURN` close-out |
| `crates/teton/src/session_ui.rs` | `SessionState::activity`, `begin_turn` arms it, `last_turn_summary`; `format_turn_summary` for BR-16 |
| `crates/teton/src/main.rs` | `FRAME_INTERVAL` import; BR-16 line on the turn arm's Ok and Err arms |
| `crates/tetond/src/runtime/mod.rs` | `@delay-ms` directive in `ScriptedFileEngine::complete`, gated |
| `crates/teton/tests/pty_e2e.rs` | AC-1, AC-2, AC-6, AC-7, AC-10 legs |
| `crates/teton/tests/cli_e2e.rs` | AC-3 pipe fixtures; AC-13 verbose fixture |
| `README.md`, `CHANGELOG.md`, `docs/` | AC-12 |

## Proposed additions to `.adlc/context/architecture.md`

- Under Key Patterns, beside "A gated surface splits into pure content and
  gated bytes": **A live row is owned by the pump that can wake** — an in-place
  terminal row is drawn, repainted, and withdrawn by exactly one loop, the one
  that holds the clock; every other writer sees the row withdrawn. The TTY gate
  is a property of the surface (`has_live_rows`), never a flag the caller
  threads (REQ-621 ADR-621-1/3).

## Lessons applied

- **LESSON-481** — pure content, gated bytes; the frame is unit-testable.
- **LESSON-568** — one seam for the close-out, region-checked (`ENDS_TURN`).
- **LESSON-569** — literal-string oracles; the frame never computes its own expectation.
- **LESSON-544** — the timing legs drive the real daemon and the real pump, not struct literals.
- **LESSON-510 / BUG-164** — every e2e leg runs under the freshness guard; rebuild both binaries before any mutation run.
- **LESSON-628** — the stall annotates what was rendered rather than reclassifying it.
- **LESSON-450** — no wait on an event alone: the stall bound is the termination path for a daemon that never speaks again.

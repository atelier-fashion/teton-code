---
id: REQ-621
title: "A live activity line while a turn is working — what the agent is doing, for how long, and that it is still alive"
status: approved
deployable: true
created: 2026-09-10
updated: 2026-09-10
component: "cli"
domain: "harness"
stack: ["rust", "cli", "json-rpc", "daemon"]
concerns: ["developer-experience", "latency", "reliability"]
tags: ["progress-indicator", "spinner", "status-line", "streaming", "tool-call", "turn", "tty", "event-bus", "loading-indicator"]
---

## Description

In an interactive `teton` session a turn is silent for most of its life. Reply
text streams as it arrives, and a tool call prints one `shell: cargo test
[running]` line and later one `[done]` line — but everything between those
moments shows nothing at all:

- the stretch between pressing Enter and the first streamed byte — routing,
  context assembly, repo-context, the remote provider's queue latency, a local
  prefill below REQ-616's progress threshold, and any adaptive "thinking" the
  provider spends before its first text token (neither provider streams
  reasoning as text, so a long think is pure silence);
- the stretch while a tool runs — a forty-second test suite is one `[running]`
  line and then a cursor that does not move;
- the stretch after a tool result while the model composes its next call.

A user watching that cursor cannot tell **working** from **hung**. The two
existing indicators cover only the edges of the problem: REQ-556's loading
indicator animates the *model-load* window before the first turn, and REQ-580's
`turn_queued` notice explains a turn *held* for a warming local tier. Neither
says anything once a turn is actually running.

Claude Code's CLI answers this with a single **activity line** pinned beneath
the output: a spinner, a verb for the current activity ("Thinking…",
"Running `cargo test`…"), the seconds elapsed, and nothing more. It is replaced
in place, never scrolls, and vanishes when the turn ends. This REQ brings the
same thing to `teton`: one row that is present whenever the turn is in a silent
phase, names that phase honestly from what the daemon has reported, counts the
seconds, and keeps moving so the user knows the process is alive.

Why it matters here specifically: Teton's product promise is cost control. A
user who believes a turn has hung kills the process — after the frontier call
has already been paid for — and re-runs it. An activity line that shows
"waiting on claude-opus-5 (summit) · 23s" turns that into a decision made with
information instead of a guess.

## System Model

_The activity line is a **projection** of daemon events plus a local tick. The
client holds no session fact the daemon lacks (architecture BR-4): every field
below is derived from an event the daemon already published, or from the
client's own clock._

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| TurnActivity | phase | enum | required; one of `idle`, `preparing`, `awaiting_model`, `streaming`, `tool_running`, `awaiting_permission`, `held`, `compacting` |
| TurnActivity | stalled | boolean | derived; true when `last_event_tick` is older than the quiet bound and the phase is not `tool_running`. An annotation on the phase, never a replacement for it (amended 2026-09-10, ADR-621-5) |
| TurnActivity | detail | string | optional; the model id and tier band for `awaiting_model` / `streaming`; the daemon-composed tool title for `tool_running`; the daemon's own sentence for `held`. Never composed by the client from raw arguments |
| TurnActivity | phase_since_tick | number | required; the tick at which the current phase began. Elapsed-in-phase is derived from it |
| TurnActivity | turn_since_tick | number | required; the tick at which the prompt was submitted. Elapsed-in-turn is derived from it |
| TurnActivity | last_event_tick | number | required; the tick of the most recent daemon event for this session. Drives `stalled` |
| TurnActivity | cost_so_far | number | required; the sum of this turn's `cost_recorded` rows, starting at zero. Exact, never estimated |
| TurnActivity | session_id | string | optional; the client's own session, used to ignore other sessions' events on the daemon-wide bus |
| ActivityFrame | text | string | the rendered row, or absent when nothing should be drawn; fits the surface width; a pure function of `(TurnActivity, tick)` with no clock, no I/O, no terminal |

### Events

_Consumed (all already on the wire; none changed)._

| Event | Trigger | Payload |
|-------|---------|---------|
| `session/prompt` sent (client-local) | the user submits a prompt | marks `turn_since_tick`; phase → `preparing` |
| `route_decided` | the daemon chose a model for this turn | model id, tier band; phase → `awaiting_model` |
| `session_update: agent_message_chunk` | reply text arrived | text; phase → `streaming` |
| `session_update: tool_call` (`in_progress`) | a tool started | tool title; phase → `tool_running` |
| `session_update: tool_call_update` (`completed` / `failed`) | a tool finished | phase → `awaiting_model` (the model is composing its next step) |
| `permission_request` | the daemon is waiting on the user | phase → `awaiting_permission` |
| `turn_queued` | the turn is held for a warming tier | the daemon's sentence; phase → `held` |
| `prefill_progress` | a long local prefill is under way | tokens done / total; stays `awaiting_model`, detail carries the fraction |
| `cost_recorded` | a model call in this turn was billed | the row's amount; adds to `cost_so_far`, phase unchanged |
| `context_compacted` | the daemon compacted context mid-turn | phase → `compacting` until the next event |
| `session/prompt` returned, or failed by any path | the turn ended | phase → `idle`; the row is removed |
| local tick | the client's frame interval elapsed | advances the spinner and the elapsed counters; sets `stalled` past the quiet bound |

_Produced: none required by this spec. If the architecture finds that an
existing event does not mark one of the transitions above — the most likely
gap is "model request issued" after a tool result, which today is inferred from
`tool_call_update` — a **new additive event** may be introduced under BR-14.
The wire shape is the architecture phase's decision._

## Business Rules

_Explicit, testable constraints governing this feature's behavior._

- [ ] BR-1: **Present in every silent phase of a turn.** In an interactive (TTY) session, from the moment a prompt is submitted until the turn ends, exactly one activity row is visible whenever the phase is anything other than `idle`, `streaming`, or `awaiting_permission`. During `streaming` the row is withdrawn — the arriving text is the liveness signal. During `awaiting_permission` it is withdrawn because the permission prompt owns the terminal and its own question is the indication (BR-9, BR-10); the phase is still tracked for BR-16. The row returns the instant the next silent phase begins (a tool starts, the stream ends without ending the turn, the permission is answered).
- [ ] BR-2: **Honest phase, daemon-supplied detail.** The row names the phase from a fixed vocabulary and fills detail only from event payloads: the provider, tier, and — when the event names one — the model from `route_decided`, the tool title the daemon composed from `tool_call`, the sentence the daemon wrote from `turn_queued`. The client never re-derives a title from raw arguments and never names a phase the daemon has not reported — before `route_decided` the row says it is preparing, not which model it is waiting on (informed by REQ-580 BR-7, LESSON-456).
- [ ] BR-3: **Elapsed time is measured, never estimated.** The row shows seconds elapsed in the current phase and in the turn, from the client's own clock. It shows no ETA, no countdown, and no fraction unless the daemon supplied one (`prefill_progress`). A number the client cannot measure does not appear (informed by REQ-556 BR-5). The one figure beyond time the row carries is **cost so far**: the exact sum of this turn's `cost_recorded` rows, shown once it is non-zero, in the same units and formatting the cost meter uses. A live token count is an estimate and is never shown.
- [ ] BR-4: **It keeps moving during silence.** During any stretch in which the daemon sends nothing, the row still advances at least once per second — spinner and elapsed counter both. The client being blocked on the daemon's socket is not an acceptable reason for the row to freeze; the mechanism that guarantees the wake (a client-side timeout, a daemon-side keepalive, or another) is the architecture phase's choice.
- [ ] BR-5: **One row, repainted in place, gone without a trace.** The row occupies one terminal row, is repainted rather than appended, and never accumulates into scrollback. Durable lines that arrive during a turn (`shell: … [running]`, `[done]`, a permission prompt, a notice) print in their usual place and the row moves beneath them. When the turn ends the row is erased, so the session's scrollback after the turn is byte-identical to what it would have been without this feature, apart from BR-16's single verbose-mode line.
- [ ] BR-6: **TTY-gated, byte-identical when piped.** With stdout not a terminal the feature emits nothing — not a frame, not an escape, not a blank line — and, outside verbose mode, piped output for any turn is identical to today's; in verbose mode the only difference is BR-16's line (informed by REQ-556 BR-2, REQ-560 BR-9).
- [ ] BR-7: **Content is a pure function of state; only bytes are gated.** The frame's text is computed from `(TurnActivity, tick)` by a function with no clock, no I/O and no terminal, so it is unit-tested with the TTY gate out of the way; the gate decides only whether the bytes are written (informed by REQ-556 BR-11, REQ-560 BR-8, LESSON-481).
- [ ] BR-8: **Through the `Surface` seam, never direct to stdout.** Every byte the row produces goes through the existing surface abstraction, so a future front-end inherits the row by implementing the same seam and no rendering path bypasses the sanitizer (informed by REQ-556 BR-3, REQ-560 BR-12).
- [ ] BR-9: **Never blocks, delays, or consumes input.** The row's cadence adds no latency to receiving or rendering daemon events, and text typed while the row is animating is delivered intact to the next prompt; a repaint never blanks characters the user has echoed (informed by REQ-556 BR-4).
- [ ] BR-10: **No second renderer.** The tool status lines, the prefill bar, the loading indicator, the held-turn notice, and the permission prompt keep their existing rendering. The row composes beside them and never restates their content in a second style (informed by REQ-556 BR-10).
- [ ] BR-11: **A stall is named, not disguised.** When no daemon event has arrived for longer than the quiet bound, the row keeps naming the phase the daemon **last reported**, stops its spinner, and appends how long it has been since the daemon last spoke. It keeps counting and the annotation stays until a real event arrives. A wedged daemon must look different from a slow model (informed by LESSON-450, LESSON-628). **The quiet bound is 15 seconds.** Streaming is not exempt: if reply bytes stop mid-stream for longer than the bound, the row returns beneath the partial reply with the annotation and is withdrawn again when bytes resume. `tool_running` **is** exempt: the daemon publishes nothing while a tool runs, so silence there is the expected state and the tool's own elapsed counter is the signal. *(Amended 2026-09-10 at architecture, ADR-621-5: the first draft replaced the phase with `stalled`, which would have relabelled every long-running tool as a stall at 15 s.)*
- [ ] BR-12: **Every exit path removes the row.** A turn that ends by a normal result, an RPC error, a transport error, a daemon disconnect, or a refused permission leaves no row behind. Termination does not depend on the daemon sending a final event.
- [ ] BR-13: **A rendering failure is never fatal and never silent.** A terminal error while painting the row abandons the row for the rest of the turn, leaves the turn itself untouched, and is recorded in verbose output (informed by REQ-556 BR-9, REQ-560 BR-13).
- [ ] BR-14: **Additive on the wire.** No existing method or event changes shape. Any new event this feature needs is additive; `PROTOCOL_VERSION` is unchanged and a client that does not know the event ignores it (informed by REQ-580 BR-8).
- [ ] BR-15: **This client's turn only.** The bus is daemon-wide. Events carrying another session's id do not change this client's row; an event with no session id is treated as ours, matching the reading the reply accumulator already takes (informed by REQ-567).
- [ ] BR-16: **A non-visual counterpart at turn end.** In verbose mode the client prints one durable line when a turn ends, giving the turn's total elapsed time, the time spent in model phases, the time spent in tools, and the cost so far — the same figures the row showed, from the same accumulator. This is the row's non-visual read path: it is emitted whether or not stdout is a terminal, so a piped verbose session can recover what the row would have shown (informed by REQ-560 BR-10). Outside verbose mode nothing is printed, so default piped output stays byte-identical (BR-6).

## Acceptance Criteria

- [ ] AC-1: **Silent lead-in is visible.** PTY e2e: against a stub daemon that delays the first reply byte by three seconds, the row appears within one frame interval of Enter in the `preparing` phase, moves to `awaiting_model` naming the stubbed model and tier when `route_decided` arrives, its elapsed counter advances at least twice before the first byte, and it is withdrawn when streaming begins.
- [ ] AC-2: **A running tool is visible.** PTY e2e: a stubbed tool that runs for three seconds prints the durable `[running]` line, shows the row beneath it in `tool_running` with the tool's title and elapsed seconds, and on completion prints `[done]` and moves the row to `awaiting_model`. With the stub emitting a `cost_recorded` row for the first model call before the tool starts, the row shows that exact amount as cost so far while the tool runs — a single-call turn cannot show it, because its only cost row arrives as the turn ends.
- [ ] AC-3: **Piped output is unchanged.** Every existing non-verbose pipe-mode fixture for a scripted turn passes byte-for-byte; every existing verbose fixture differs by exactly BR-16's line and nothing else; and a new fixture that drives the AC-1 and AC-2 scripts with stdout piped produces no indicator bytes.
- [ ] AC-4: **Frames are pure and the animation can fail.** A unit table maps `(phase, detail, elapsed)` to exact frame strings with no terminal involved. The mutation "the frame ignores its tick" is applied and observed to fail the animation test; the mutation is recorded in the test's doc comment. The oracle does not call the frame function to compute its expectation (informed by REQ-556 AC-8, LESSON-569).
- [ ] AC-5: **Transitions are driven by the real producer.** For every event the row consumes, a cross-seam test drives the daemon's actual publisher and asserts the row's phase — not a hand-built struct literal alone. Any new event introduced under BR-14 has the same test (informed by LESSON-544).
- [ ] AC-6: **A stall is named.** PTY e2e: a stub daemon that goes silent mid-turn for longer than the quiet bound causes the row to keep its last phase and add the stall annotation with seconds since the last event; the spinner stops; a subsequent real event clears the annotation. A tool that runs longer than the bound never shows the annotation. A second leg stops the stub mid-stream for longer than the bound and asserts the row returns beneath the partial reply and is withdrawn when bytes resume.
- [ ] AC-7: **No stuck row.** PTY e2e covers each exit path in BR-12 — normal result, RPC error, stub killed mid-turn — and asserts the row is erased and the scrollback carries no residue.
- [ ] AC-8: **No invented phase.** A scripted event sequence with no `route_decided` never renders a model name; a sequence with no `tool_call` never renders `tool_running`; a stall never renders a phase the daemon did not report.
- [ ] AC-9: **Other sessions are ignored.** Events tagged with a different session id leave the row's phase and counters unchanged.
- [ ] AC-10: **Typing survives the animation.** PTY e2e: bytes typed while the row is animating arrive intact at the next prompt, and no repaint blanks them.
- [ ] AC-11: **The e2e legs are honest.** Every PTY leg above runs under the harness's binary-freshness guard so a stale daemon cannot green a leg (informed by BUG-164, LESSON-510), and every TTY claim in this list has a real PTY test, not a renderer-unit stand-in (informed by BUG-191).
- [ ] AC-12: **Documented.** The user docs describe the activity line, its phase vocabulary, the cost-so-far figure, the stall wording, and the verbose end-of-turn summary, and state that piped use shows none of the row.
- [ ] AC-13: **The verbose summary matches the row.** A verbose-mode turn ends with one line carrying total, model, tool, and cost figures; a unit test asserts they are read from the same accumulator the frames read, and a pipe-mode fixture shows the line is present under verbose and absent otherwise.

## External Dependencies

- None. The requirement introduces no new service, API, or library; whether a crate is used for terminal cursor control is an architecture decision.

## Assumptions

- The client's receive loop blocks on the daemon socket for the whole of a `session/prompt` call today, which is why nothing can animate mid-turn. BR-4 is satisfiable by a client-side wake or a daemon-side keepalive; the spec deliberately does not choose.
- Neither provider adapter streams reasoning tokens as text, so `awaiting_model` is the honest phase for a model that is thinking. If a provider later exposes reasoning deltas, a `thinking` phase is an additive extension of BR-2's vocabulary.
- The existing `route_decided`, `session_update`, `permission_request`, `turn_queued`, `prefill_progress`, and `context_compacted` events are sufficient to distinguish every phase in BR-2, with the possible exception of "model request issued" after a tool result, which BR-14 leaves room to add.
- The projection is designed so a future thin client (the VS Code extension) can reuse it, but no extension work is in this REQ.

## Open Questions

- [x] OQ-1: Running cost in the row — **resolved 2026-09-10: yes, cost so far**, the exact sum of this turn's `cost_recorded` rows; never a token estimate (BR-3).
- [x] OQ-2: A stall inside a stream — **resolved 2026-09-10: yes**, streaming is not exempt from the stall rule (BR-11).
- [x] OQ-3: Quiet bound — **resolved 2026-09-10: 15 seconds** (BR-11). At architecture the stall became an annotation on the last reported phase, with `tool_running` exempt (ADR-621-5).
- [x] OQ-4: Non-visual counterpart — **resolved 2026-09-10: yes**, a verbose-mode end-of-turn timing and cost line (BR-16, AC-13).

## Out of Scope

- Cancelling a turn from the keyboard. No cancel method is exposed by the CLI today, so the row advertises no cancel key; that is a separate REQ.
- Any change to the daemon's turn path timing, or streaming of reasoning tokens from a provider.
- VS Code extension rendering.
- A full-screen or alternate-screen UI; the row is one line through the existing surface.
- Replacing or restyling the existing tool status lines, prefill bar, loading indicator, held-turn notice, or permission prompt.
- ETAs, percentages, or progress fractions the daemon did not supply.
- A live token meter in the row; cost so far is in scope, a token estimate is not (OQ-1).

## Retrieved Context

- BUG-189 (bug, score 13): Two refusal reasons publish no record, so the session surface never says why
- REQ-560 (spec, score 13): Named permission levels and the interactive session status line
- BUG-164 (bug, score 11): A targeted e2e run can pass against a stale daemon binary
- BUG-191 (bug, score 11): AC-6 and AC-14 claim a pty leg for the acknowledgment prompt bytes; the pty suite has none
- LESSON-510 (lesson, score 11): A harness that checked a binary exists has not checked it is the one under test
- LESSON-568 (lesson, score 10): An ADR's causal sentence is exactly as unverified as an untested line of code
- REQ-584 (spec, score 10): A project locator — the session can name this machine's projects without walking the disk, and a bare name moves the root
- REQ-583 (spec, score 10): Session-root awareness and bounded discovery
- REQ-556 (spec, score 10): Live model-loading progress in the interactive session
- REQ-567 (spec, score 10): Cross-prompt conversation carry in interactive sessions
- REQ-615 (spec, score 9): Session-root honesty for the shell tool and skill preambles
- REQ-617 (spec, score 9): The model knows the session's own commands and stops repeating itself
- LESSON-628 (lesson, score 9): Announce on what was rendered, not on what was stored
- LESSON-569 (lesson, score 9): Seven assertions that passed but could not fail — and the three ways they got that way
- LESSON-544 (lesson, score 9): A test that builds the wire value by hand leaves the line that produces it unguarded

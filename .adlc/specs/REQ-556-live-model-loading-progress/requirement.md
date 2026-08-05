---
id: REQ-556
title: "Live model-loading progress in the interactive session"
status: complete
deployable: true
created: 2026-08-04
updated: 2026-08-04
component: "cli"
domain: "clients"
stack: ["rust", "cli", "json-rpc"]
concerns: ["developer-experience", "observability"]
tags: ["loading-window", "progress-indicator", "model-lifecycle", "event-stream", "first-run", "tty"]
---

## Description

For roughly forty seconds after every daemon start, a `teton` session shows the
user nothing. The local tier is deep-verifying, loading and benchmarking
multi-gigabyte weights, and the session sits at a `›` prompt that looks
completely idle. The only way to find out what is happening is to type a prompt
and be told no.

BUG-146 made that refusal name the right state; BUG-152 made it render as a
notice rather than an error. Both improved the answer to a question the user
should not have had to ask. This REQ removes the question: the session says
what it is doing while it does it, and announces when the tier opens, without
being prompted.

Two distinct defects compound to produce today's silence, and the first is the
load-bearing one:

1. **The client cannot see events while idle.** A background reader thread
   already delivers daemon events into a channel, but they are only *rendered*
   while the client is inside an RPC call. Between turns the entry loop blocks
   on `stdin`, so lifecycle events queue unrendered. This is observable: in the
   reported session the `benchmark …: first token 368 ms, 73.0 tok/s` and
   `local model … ready` lines appeared only *after* the user typed a line that
   triggered a turn. The tier had been ready for some time and the session had
   no way to say so. **Even with no animation at all, `>> local model … ready`
   should land when it happens.**

2. **One window has nothing to show even when events do flow.** Every lifecycle
   stage already has a rendering — `render_lifecycle` turns each into a one-line
   notice, and download already gets a real progress bar. But between the
   verify and the benchmark the daemon publishes *nothing at all*, and that
   silence is most of the wait. Only there does a continuous, client-driven
   indicator have to invent motion the daemon does not supply.

Scope follows from that split, and it is narrower than it first appears: the
per-stage lines are already written and stay untouched (BR-10). This REQ moves
**when** they render, and draws motion in the **one** window that has no line.

The user-visible outcome: `model starting` with growing dots (or an equivalent
indeterminate motion) while the tier comes up, real percentage progress during
download where the byte counts exist, and the existing `ready` line arriving on
its own.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| LoadingIndicator (client-side, ephemeral; persists nothing) | `phase` | enum: at least `working` and `hidden` | derived only from received `model_lifecycle` events — never inferred from elapsed time. The exact variant set is `/architect`'s call |
| | `model_id` | string | as published by the daemon; never a filesystem path (REQ-547 BR-11) |
| | `tick` | monotonic counter | the only thing a timer advances; what makes the frame sequence a pure function of state (BR-11) |

The field list is deliberately thin. An earlier draft specified `determinate`,
`fraction`, `frame` and `visible` — that was this document designing a struct,
which is `/architect`'s job and partly pre-answers OQ-1. What the requirement
constrains is the *observable behaviour* (BR-5, BR-11), not the shape that
produces it.

### Events

Consumed, not defined — this REQ adds no protocol surface (BR-8). All stages
below already exist on `model_lifecycle`:

**Every stage below already has a rendering.** `render_lifecycle`
(`crates/teton/src/firstrun.rs`) turns each one into a one-line notice today,
including `Download` through `progress_bar`, which already handles the
`Option<u64>` total. This REQ therefore adds **no new per-stage line** (BR-10)
— it changes *when* these lines reach the user, and fills the one window that
publishes nothing at all.

| Event | Payload | Rendered today | What this REQ changes |
|-------|---------|----------------|-----------------------|
| `Download` | `downloaded_bytes`, `total_bytes: Option<u64>` | `download <id>: <bar>`, determinate when total is known | **nothing** — line and bar unchanged; only its timeliness (BR-1) |
| `Verifying` | `total_bytes` | `verifying <id> (<n> bytes)` | **nothing** — timeliness only |
| `Benchmark` | `first_token_ms`, `tokens_per_sec` | `benchmark <id>: first token … tok/s` | **nothing** — timeliness only; ends the animated window |
| `Ready` | — | `local model <id> ready` | **nothing** — timeliness only; clears the indicator |
| `AwaitingDecision` | `reason` | `local model <id> awaiting your decision: …` | no indicator — nothing is in progress (BR-6) |
| `SteppedDown`, `Disabled` | reason fields | rendered as today | clears the indicator (BR-6) |
| *(no event)* | — | **nothing at all** | **the whole of this REQ's new output**: indeterminate motion after `Verifying` and before `Benchmark`/`Ready` |

**There is no `Loading` stage.** Between `Verifying` and `Benchmark` the daemon
publishes nothing, which is precisely the window this REQ is about — the last
row above is the only one where new bytes are drawn. The client knows only
"verified, not yet ready", the same fact `loading_local_engine_reason` states.
That absence is why BR-5 forbids a countdown rather than merely discouraging
one.

## Business Rules

- [ ] BR-1: In an **interactive (TTY) session**, the entry loop must render
      daemon events **while idle at the prompt**, not only while inside an RPC.
      A lifecycle event arriving while the user is typing nothing must reach the
      surface at the time it arrives. This is the foundation; every other rule
      here is decoration on top of it. (informed by BUG-152, LESSON-456)

      **Non-TTY sessions keep today's behaviour** — events render when the next
      RPC pumps them — which is what lets BR-2's byte-identical guarantee hold.
      Without this gate BR-1 and BR-2 are unsatisfiable together: rendering an
      event "when it arrives" changes *when bytes are emitted* on a pipe too.
      Stated residual: a piped session still learns the tier opened only at its
      next turn. Accepted — nobody is watching a pipe, and a pipe's consumer is
      parsing bytes whose order it was promised.
- [ ] BR-2: The indicator is **TTY-gated**. With stdout not a terminal, no
      animation, no cursor control, and no indicator frames are emitted, and the
      session's output stays byte-identical to the pre-REQ binary. (informed by
      REQ-549, REQ-555)
- [ ] BR-3: All indicator output goes through the existing `Surface` seam — no
      direct-to-stdout side channel — so a future ratatui front-end inherits the
      behaviour by implementing the same seam. (informed by REQ-549, REQ-555)
- [ ] BR-4: The indicator must never block, delay, or consume input. Typing
      during the load window behaves exactly as it does today, and `Ctrl-D`,
      `/quit` and `/exit` still leave through the same path. (informed by
      BUG-153)
- [ ] BR-5: Progress claims must be **honest per phase**. A fraction or
      percentage may be shown only where the daemon supplies the byte counts to
      compute it; the load and benchmark window shows indeterminate motion and
      **no ETA, countdown, or estimated remaining time**. No elapsed-time
      heuristic may be presented as prediction.
- [ ] BR-6: The indicator represents *work in progress only*. States that are
      waiting on the user (`AwaitingDecision`), settled against the machine
      (`Disabled`), or terminal (`SteppedDown`, a failed load) must not animate
      — an indicator implies something is happening. (informed by BUG-152)
- [ ] BR-7: The indicator must have a **termination path that does not depend on
      receiving a `Ready` event**. The daemon publishes `ready` before the
      runtime applies it, and a client can attach inside that gap and truthfully
      never hear another event — so "spin until `Ready` arrives" can spin
      forever. The indicator must resolve against a state-derived surface or a
      bounded condition, and say what it knows when it does. (informed by
      LESSON-450)
- [ ] BR-8: No new protocol surface. The indicator is a new **consumer** of
      `model_lifecycle` as already published; if a phase genuinely cannot be
      rendered honestly without a new daemon event, that is an explicit scope
      decision recorded at architecture time, not an implementation detail.
      (informed by REQ-555)
- [ ] BR-9: A failure to render the indicator is never fatal and never silent.
      If animation cannot be driven (terminal too narrow, cursor control
      unavailable, write error), the session degrades to the existing static
      notice and remains fully usable — the loading state stays discoverable by
      the fallback, not lost with the decoration. (informed by LESSON-447)
- [ ] BR-10: **No second renderer.** The per-stage lines stay
      `render_lifecycle`'s, byte for byte. This REQ changes when those lines
      reach the user and adds motion to the one window that has no line at all;
      it must not introduce a parallel rendering of any stage that already has
      one. Two renderings of one daemon state is the drift this project has
      repeatedly paid for. (informed by REQ-555, LESSON-456)
- [ ] BR-11: The indicator's output must be **derivable from state without a
      terminal**: given a sequence of lifecycle events and ticks, the frames it
      would draw are computable and assertable in a unit test. BR-2 makes the
      indicator invisible to every piped test, so without this rule the
      feature's core behaviour has no verification path at all — the TTY gate
      would double as a test blindfold. (informed by LESSON-441)

## Acceptance Criteria

AC-1 through AC-6 each name how they are verified, because BR-2 puts the
indicator outside the reach of the existing piped harness. Three routes are
used: **unit** (the BR-11 state→frames function, no terminal), **pty** (a new
PTY-driven e2e — see External Dependencies), and **piped** (the existing
`cli_e2e` harness). AC-7 and AC-8 carry no route tag because they are
cross-cutting claims about the suite as a whole — which platforms it ran on,
and whether it actually fails when the feature is disabled.

- [ ] AC-1 *(unit + pty)*: With the daemon mid-load, opening `teton` shows a
      live indicator naming the model and its current phase, advancing at a
      steady interval, **with no input typed**.
- [ ] AC-2 *(pty)*: When the tier opens, the indicator clears and the existing
      `>> local model <id> ready` line renders **on its own**, with no prompt
      typed and no RPC issued. This is the direct regression test for the
      queued-event defect and fails against today's binary. It is the one
      criterion that cannot be reached any other way — it is *about* timing —
      which is what justifies the PTY harness.
- [ ] AC-3 *(unit + existing tests)*: The per-stage lines are unchanged.
      `render_lifecycle`'s and `progress_bar`'s existing unit tests pass
      **unmodified** (BR-10). New output appears only in the window that
      publishes nothing — indeterminate motion, and no ETA, countdown, or
      estimated remaining time anywhere in the session (BR-5).
- [ ] AC-4 *(piped)*: Piped (non-TTY) invocation produces output byte-identical
      to the pre-REQ binary. `slash_quit_ends_the_session_exactly_as_ctrl_d_does`
      and the `/exit` equivalence leg pass **unmodified** — a test edited to
      accommodate indicator bytes is a BR-2 violation, not an accommodation.
- [ ] AC-5 *(pty)*: A prompt typed while the indicator is running is accepted
      intact, the entry frame is not corrupted, and the turn proceeds — or
      returns `TIER_WARMING` and renders as BUG-152 defined.
- [ ] AC-6 *(unit)*: Driven with a lifecycle sequence that never reaches
      `Ready`, the indicator terminates on its bounded condition rather than
      advancing indefinitely, and the line it leaves behind names the last state
      it actually observed (BR-7).
- [ ] AC-7: Verified on **both** macOS and Linux in CI. TTY detection and
      terminal-width handling are platform-specific and a green macOS run is not
      evidence about Linux. (informed by LESSON-433)
- [ ] AC-8: Mutation check — freezing the animation counter, or removing the
      idle-render path, fails at least one test. A suite that stays green with
      the feature disabled has not tested it. (informed by LESSON-441)

## External Dependencies

- **A PTY test harness — one new dev-dependency.** The workspace has none: the
  `cli_e2e` suite drives the binary over pipes, and BR-2 makes the indicator
  render nothing on a pipe. AC-2 and AC-5 are about behaviour *at* a terminal
  and cannot be reached any other way, so a pty crate enters the dev-dependency
  set. Named here rather than discovered at implementation time, because "the
  ACs are untestable" is the kind of thing that otherwise surfaces as a quietly
  dropped criterion. Everything BR-11 covers is unit-testable without it.
- Runtime: none expected. The event transport, the reader thread, the `Surface`
  seam and the TTY/width detection all already exist. If architecture concludes
  a terminal-control crate is warranted, that is an ADR decision recorded
  there — this REQ does not assume one.

## Assumptions

- `model_lifecycle` already carries what determinate progress needs:
  `Download { downloaded_bytes, total_bytes: Option<u64> }` and
  `Verifying { total_bytes }` are present in `teton-protocol`, and REQ-547 AC-2
  already requires progress rendered from these events during the consent flow.
  **Verified against the protocol source, not assumed.** (informed by REQ-547)
- **Not an assumption — a verified fact that narrowed this REQ.** The client
  already renders every lifecycle stage: `render_lifecycle`
  (`crates/teton/src/firstrun.rs`) covers `Probed`, `Download` (with
  `progress_bar`, which already branches on `Option<u64>`), `Verifying`,
  `Benchmark`, `AwaitingDecision`, `Ready`, `SteppedDown` and `Disabled`. The
  first draft of this spec claimed that rendering as new work across a whole
  section; validation caught it against the source. BR-10 now forbids
  duplicating it. (informed by REQ-547, REQ-555)
- The load-and-benchmark window publishes no intermediate progress. Confirmed by
  the absence of any `Loading` stage between `Verifying` and `Benchmark`. If a
  future daemon change adds one, BR-5's prohibition on synthesised progress can
  be relaxed for that phase — and only that phase.
- Events already arrive asynchronously on a background reader thread into a
  channel, so BR-1 is a change to *when the client drains and renders*, not to
  the transport. The daemon needs no change to satisfy BR-1.
- The user's terminal honours the ANSI cursor control the existing framed
  prompter already relies on. This REQ inherits that assumption rather than
  adding it.

## Open Questions

- [ ] OQ-1: Does the entry loop become non-blocking (poll `stdin` and the event
      channel together), or does a render thread paint while the prompter still
      blocks? The first keeps a single thread owning the terminal, which is why
      it is the leading candidate; the second is less invasive but two writers
      to one terminal is exactly how the framed entry area gets corrupted.
      Architecture decision, and the one everything else depends on.
- [ ] OQ-2: Where does the indicator sit relative to the framed entry area —
      above the top rule, in place of the prompt line, or on a reserved row? This
      determines whether a redraw can ever collide with a partially typed line.
- [ ] OQ-3: BR-10 settles the *rendering* half — the stage lines are reused, not
      re-written. What remains open is the **animated** indicator during the
      first-run download, where the consent flow currently owns the screen: does
      motion appear there too, or only in the post-consent load window this REQ
      is named for? Download already has a determinate bar, so motion adds
      little there and risks disturbing a shipped consent flow.
- [ ] OQ-4: What is BR-7's bounded condition concretely — a poll of a
      state-derived surface (`model/status`), a timeout, or both? A timeout alone
      would print "still loading" forever on a genuinely wedged daemon.

## Out of Scope

- **A countdown or ETA to ready.** No data source exists: the load window
  publishes nothing, and load duration is not recorded across runs. A countdown
  that reaches zero while the user is still waiting is worse than no countdown
  (BR-5). Recording load durations to enable a future honest estimate is a
  separate REQ.
- A full-screen TUI / ratatui migration. The `Surface` seam is written against
  that future (BR-3), but this REQ stays line-based.
- Progress indication for remote provider calls or for turn execution.
- New `model_lifecycle` stages or any other daemon protocol change (BR-8).
- Windows support.

## Retrieved Context

Retrieval fired with `component: cli`, `domain: clients`,
`stack: [rust, cli, json-rpc]`,
`concerns: [developer-experience, observability]`,
`tags: [loading-window, progress-indicator, model-lifecycle, event-stream, first-run, tty]`.

| ID | Corpus | Score | Title |
|---|---|---|---|
| REQ-555 | spec | 10 | In-session slash commands for the teton interactive CLI |
| BUG-146 | bug | 8 | First prompt after install fails with a message blaming the local engine |
| BUG-152 | bug | 7 | A prompt typed while the local tier is still loading is reported as an error |
| LESSON-456 | lesson | 6 | A `_`-discarded error is a silent downgrade |
| REQ-547 | spec | 6 | First-run local model consent |
| BUG-153 | bug | 4 | /exit is not a command |
| LESSON-475 | lesson | 3 | A marker must be anchored the way the renderer actually writes it |
| REQ-554 | spec | 3 | Local tier renders prompts through the model's native chat template |
| REQ-548 | spec | 3 | One-command Homebrew install and the tetoncode.ai landing page |
| REQ-550 | spec | 3 | Stable code-signing identity and build provenance |
| LESSON-457 | lesson | 3 | An executable's filename is a trust surface |
| REQ-549 | spec | 3 | Daemon process identity and interactive startup UX |
| LESSON-441 | lesson | 3 | A fix pass is new code — re-verify it adversarially |
| LESSON-447 | lesson | 3 | A best-effort fallback must preserve the invariant it backs up |
| LESSON-433 | lesson | 3 | Single-platform local verification gives false confidence |

Two notes on how this list was produced, so a later reader can reproduce it:

- **Spec status filter.** The retrieval contract admits specs with status
  `approved`, `in-progress` or `deployed`. Every spec in this project uses
  `complete` as its terminal status, so a literal filter would have excluded all
  eight and left retrieval with lessons and bugs only. `complete` was treated as
  the local spelling of `deployed`. Recorded rather than silently applied.
- **One near-miss read anyway.** `LESSON-450` (an event published before the
  state applies is not a sync point) scored 2 and fell outside the top 15, but
  its publish-then-apply finding is directly load-bearing for BR-7 — the `ready`
  event this feature would naturally stop on is broadcast *before* the runtime
  flips the tier open. It was read directly and is cited above.

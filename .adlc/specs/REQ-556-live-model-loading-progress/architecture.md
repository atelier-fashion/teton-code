# REQ-556 — Architecture

## Approach

The requirement splits cleanly into a **transport-shaped** problem and a
**rendering-shaped** one, and the first is load-bearing:

1. The entry loop's blocking point is `io::stdin().read_line` inside
   `FramedStdinPrompter::ask` (`crates/teton/src/prompt.rs`). While parked
   there, nothing drains `Connection::incoming`, so lifecycle events sit in the
   channel until the next `conn.call` pumps them (`client.rs:155`). Everything
   BR-1 asks for follows from moving that blocking point.
2. Once events flow, only one window still has nothing to say — between
   `Verifying` and `Benchmark` the daemon publishes no event at all — so motion
   there must be client-driven.

The codebase supplies an unusually clean answer to the first. `Incoming`
(`client.rs:89`) is already a multi-variant enum fed by a single
`mpsc::Sender` from the reader thread. Adding a second producer to that same
channel turns "an event arrived" and "the user typed a line" into one blocking
receive — which is what lets a single thread own the terminal.

## Key decisions

### ADR-556-1: One reader of stdin; the *wait* is interruptible, not the read

**Superseded draft (recorded, not deleted).** The first version of this ADR put
the blocking `read_line` on a **stdin reader thread** feeding the existing
`mpsc::Sender`, so the loop could block once on a unified channel. Writing
TASK-038 showed that plan has a defect that would have shipped as intermittent
data loss, so it is recorded here rather than quietly replaced.

**Why it fails.** `Connection::dispatch_event` (`client.rs:206`) answers
permission requests and model proposals by calling `ctx.prompter.ask()`, which
does its own blocking `read_line` on stdin. A stdin reader thread is a *second*
reader of the same descriptor, so a line typed while a consent prompt is open
goes to whichever reader the kernel wakes — the prompt or the entry loop, at
random. That is REQ-547's entire surface, and the failure would be rare,
timing-dependent, and invisible in a piped test.

**Decision.** Keep exactly **one** reader of stdin — the main thread — and make
the *wait* interruptible instead of the read. In interactive TTY sessions the
entry loop becomes:

| Condition | Loop does |
|---|---|
| queued events (`try_recv`) | render each through the existing `dispatch_event`, fold lifecycle stages into the indicator, repaint |
| `poll(STDIN_FILENO, FRAME_INTERVAL)` reports readable | `read_line`, then the existing classify → dispatch → `prompt/turn` path |
| readable and `read_line` yields 0 bytes | EOF — break to the existing post-loop cost summary (REQ-555 BR-6) |
| poll times out | advance the tick and repaint the indicator |

**Rationale.** `libc` is already a direct dependency of the `teton` crate, used
exactly this way for `TIOCGWINSZ` in `prompt.rs:114`, so `poll` on fd 0 adds no
dependency and follows an established local pattern. In canonical mode the
terminal delivers whole lines, so a `read_line` issued *after* poll reports
readable does not meaningfully block. Crucially the `Prompter` seam is
untouched: permission and proposal prompts keep reading stdin exactly as they
do today, because nothing else ever is.

This is strictly smaller than the superseded plan — no new thread, no new
`Incoming` variant, no change to the channel — which is the second reason to
prefer it.

**Consequences.**
- The **piped path is not touched at all.** Non-TTY sessions keep calling
  `FramedStdinPrompter::ask` exactly as today, so AC-4's byte-identity holds
  *by construction* rather than by careful matching — the new code is not on
  that path. This is the mechanical expression of BR-1's TTY scoping and BR-2.
- The poll timeout is the animation clock: no timer thread and no `sleep`
  anywhere in the loop.
- `poll` returning an error (notably `EINTR`) is treated as "not readable",
  which costs one extra tick and re-polls. It must never be treated as EOF.
- Unix-only, which matches the project's platform scope (Windows is out of
  scope in the requirement and in the charter).

### ADR-556-2: The indicator is a pure state machine; the loop only ticks it

**Decision.** A new `crates/teton/src/loading.rs` holds
`LoadingIndicator` with (a) `observe(&mut self, stage: &ModelLifecycleStage)`
and (b) `frame(&self, tick: u64) -> Option<String>`. `frame` returns `None`
when nothing should be drawn. No I/O, no terminal, no clock.

**Rationale.** BR-11. BR-2 makes the indicator invisible to every piped test,
so if the frame sequence were computed inside the render path it would have no
verification route at all — the TTY gate would double as a test blindfold. A
pure function is assertable from a plain unit test by feeding stages and ticks.

**Consequences.** `frame` is the single place BR-5 is enforced: it can only
return a fraction when the stage it observed carried byte counts, and it has no
clock to derive an ETA from even if someone wanted one. The prohibition is
structural, not a review note.

### ADR-556-3: Termination is state-derived and bounded (BR-7, LESSON-450)

**Decision.** The indicator stops when its state leaves `Working` — on any
terminal stage (`Ready`, `Benchmark`, `SteppedDown`, `Disabled`,
`AwaitingDecision`) **or** on a bounded tick cap, after which it stops
animating and leaves one static line naming the last stage it actually
observed.

**Rationale.** LESSON-450: the daemon publishes `ready` *before* the runtime
flips the tier open, and a client attaching inside that gap is truthfully
replayed "still loading" and never hears another event. "Spin until `Ready`
arrives" can therefore spin forever. The cap makes the failure mode a stale
line rather than a hung animation.

**Consequences.** Answers OQ-4 without adding a `model/status` poll, so BR-8
(no new protocol surface, no new RPC on this path) holds. If a future change
wants a true state poll, the cap is where it plugs in.

### ADR-556-4: The indicator owns one row above the entry frame

**Decision.** The indicator paints on a dedicated row immediately above the
framed entry area, repainted with save-cursor → move → `\r\x1b[K` → redraw →
restore-cursor, all through the `Surface` seam (BR-3).

**Rationale.** Answers OQ-2. In canonical mode the terminal itself echoes the
user's keystrokes at the cursor, so any repaint that does not save and restore
the cursor will land in the middle of a partially typed line. Owning a row
*above* the frame keeps the indicator out of the input row entirely.

**Consequences.** `Surface` grows a narrow capability for in-place repaint;
`PlainSurface` implements it with ANSI, and a non-TTY surface implements it as
a no-op — which is the second mechanical guarantee behind BR-2. `RecordingSurface`
records the calls, so the wiring is assertable without a terminal.

### Decisions recorded against the spec's open questions

- **OQ-1** → ADR-556-1: non-blocking entry loop via a unified channel.
- **OQ-2** → ADR-556-4: a dedicated row above the entry frame, cursor save/restore.
- **OQ-3** → **No motion during the first-run download.** That phase already
  has a determinate bar (`progress_bar`) and the consent flow owns the screen;
  adding motion there would disturb a shipped flow for no information gain. The
  indicator is scoped to the post-consent load window.
- **OQ-4** → ADR-556-3: bounded tick cap plus last-observed line; no new RPC.

## Files affected

| File | Change |
|---|---|
| `crates/teton/src/client.rs` | `Incoming::Stdin` variant; a `stdin_reader` producer; expose a `recv_timeout` entry point |
| `crates/teton/src/loading.rs` | **new** — `LoadingIndicator` state machine and `frame()` |
| `crates/teton/src/main.rs` | interactive entry loop restructured around the unified channel; piped path untouched |
| `crates/teton/src/render.rs` | `Surface` in-place repaint capability; ANSI in `PlainSurface`, no-op when not a TTY |
| `crates/teton/tests/pty_e2e.rs` | **new** — PTY-driven e2e for AC-2 / AC-5 |
| `crates/teton/Cargo.toml` | pty dev-dependency |

## Non-goals inherited from the requirement

No ETA or countdown (BR-5 — no data source exists), no second per-stage
renderer (BR-10), no new `model_lifecycle` stages or RPCs (BR-8), no ratatui
migration.

## Lessons applied

- **LESSON-450** — publish-then-apply; drives ADR-556-3's bounded termination.
- **LESSON-470** — interactive offers must be TTY-gated; ADR-556-1 gates by
  *not putting the new path in the piped code path at all*.
- **LESSON-441** — a fix pass is new code; TASK-041 mutation-checks the feature.
- **LESSON-447** — a fallback must preserve the invariant it backs up and fail
  visibly; BR-9's degradation leaves the static notice rather than silence.

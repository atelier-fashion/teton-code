---
id: BUG-163
title: "The self-approval attach test flakes on the Linux CI leg — the withholding mechanism is understood, the trigger is not"
status: open
severity: medium
created: 2026-08-12
updated: 2026-08-22
component: "daemon/session"
domain: "session-authorization"
stack: ["rust", "daemon", "json-rpc", "linux"]
concerns: ["security", "availability", "test-determinism"]
tags: ["ancestry-gate", "procfs", "attach-consent", "may-answer", "flaky-test", "linux", "REQ-570", "REQ-569"]
found_by: "CI on PR #107 (a docs-only change), 2026-08-12"
introduced_by: REQ-570
---

## Description

> **Corrected 2026-08-12 (third revision).** This report has now carried two
> root causes and **both were refuted**. PR #109 raised it to high and asserted a
> transient `/proc` read as "the actual mechanism"; Linux probing disproved that.
> Severity is back to **medium** and the root cause is back to **unknown**. The
> filename carries the original slug so links from PRs #108/#109 keep resolving.
>
> **Fourth revision, 2026-08-12: first instrumented capture.** The instrument
> built in #111/#112/#113 fired on its first opportunity and **positively
> excluded** both the ancestry seam and the zero-delivery path. See
> "First instrumented capture" below — the cause is still unnamed, but the
> search space is now much smaller and narrowed by measurement rather than
> reasoning.

**Status in one line.** A real, intermittent CI failure, captured once with
instrumentation: consent **succeeded**, and the read that times out is the one
waiting for the `session/attach` **response** afterwards. Cause still unnamed.

**The symptom.**
`tetond/tests/attach_authorization.rs::a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log`
intermittently fails on the **ubuntu-latest** leg. That it landed in *this* suite
matters on its own: these tests are the standing evidence for REQ-569's ancestry
gate and REQ-570's attestation split, and a suite that reddens intermittently
teaches everyone to re-run it — after which a real regression in that perimeter
gets re-run away with the noise.

## Reproduction Steps

Not reproduced deliberately — observed once and confirmed intermittent by
re-run. Frequency is roughly **1 in 9** on the Linux leg:

1. Push any commit and let CI run (the observed failure was on **PR #107, a
   one-file `.adlc` markdown change** that cannot touch daemon code).
2. Observe `fmt · clippy · test (ubuntu-latest)` fail while
   `fmt · clippy · test (macos-latest)` passes on the same commit.
3. Re-run the failed job on the identical SHA — it passes.

Evidence it is intermittent rather than a break: the eight preceding runs on
`main` (`18e9f72`, `177481a`, `37bf7c8`, `b8c5686`, `c8bdffd`, `09d5c06`,
`92d56e2`, `16bdd41`) were all green, macOS passed on the same run, and the
re-run of the same commit passed.

## Expected Behavior

The test settles by which frame arrives, not by how long one takes — its own
`READ_DEADLINE` doc comment says the deadline "is never the thing under test".
It should pass deterministically on both platform legs.

## Actual Behavior

```
thread 'a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log'
panicked at crates/tetond/tests/attach_authorization.rs:170:23:
no frame from the daemon within 20s (Resource temporarily unavailable (os error 11)):
it is wedged, or something is awaiting a consent decision nobody is going to make
```

`test result: FAILED. 5 passed; 1 failed` — the other five tests in the binary
passed, in well under a second each.

## Environment

- Platform: ubuntu-latest (GitHub Actions). Not observed on macos-latest.
- Version: `main` @ `cd5b358`; the test arrived with REQ-570 (`c8bdffd`).
- Failing run 1: <https://github.com/atelier-fashion/teton-code/actions/runs/31598675582/job/94120352305>
- Failing run 2 (**instrumented**, PR #116, docs-only): job 94257585202 — the
  capture recorded below

## Root Cause

### The original hypothesis, and why it is wrong

This report first proposed an ordering race in the test's own barrier:
`wait_for_client_count(1)` synchronizing on the daemon's client count rather
than on the session's consent-surface registry having dropped the departed
connections.

**Refuted by reading the disconnect path.** `server.rs` releases in this order:

1. `daemon.surfaces.release(state.id)` — the surface is deregistered
2. abort + await in-flight attach tasks
3. `daemon.grants.release(state.id)`
4. `drop(client_guard)` — and `ClientGuard::drop` is what publishes
   `ClientDisconnected { live_connection_count }` (`lifetime.rs:533`)

The count cannot fall until every departing connection has reached step 4, and
each of those passed step 1 first. So by the time the test observes
`client_disconnected` with count 1, both surfaces are already gone. **The barrier
is correctly ordered.** Also ruled out by inspection: both routing arms, and the
creator's attachment (set synchronously inside `record_created`).

Recorded rather than deleted, so nobody re-treads it.

### The second hypothesis — the withholding chain (real) and its trigger (refuted below)

> Kept because the **chain** is verified and load-bearing for any future
> investigation. Its **trigger** — the `/proc` read failure — is disproved in the
> next section. PR #109 published this section as "the actual mechanism"; that
> framing was wrong and is retracted.

**`crates/tetond/src/peer.rs`, the Linux arm:**

```rust
pub fn parent_of(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    super::ppid_from_proc_status(&status)
}
```

That `.ok()?` collapses **every** read error into `None`. Its own doc comment
says *"A missing or unreadable file is a vanished process: `None`, not a
guess"* — but those are two different facts. `ENOENT` means the process is gone.
`EACCES`, `EINTR`, `ENOMEM`, or a short read under contention mean **the read
failed while the process is alive**.

From there the chain is mechanical, and every link already exists in the code:

| Step | Where |
|---|---|
| `None` from the parent lookup | `linux::parent_of` |
| → `Ancestry::Indeterminate` — *"the chain broke under us"* | `is_descendant_of` |
| → `may_hold_session_access()` is false | `ConnState` |
| → the surface is registered with `may_answer: false` | `register_consent_surface` |
| → `deliver()` **silently skips** that surface | `consent.rs::deliver` |
| → the consent frame reaches nobody; the read blocks to its deadline | the test |

**This is the same shape as BUG-159 and [[LESSON-489]], one layer down: a read
failure treated as a fact about the world rather than a fact about the read.**
BUG-159 conflated "the file vanished" with "the read failed"; this conflates
"the process vanished" with "the `/proc` read failed."

### REFUTED — the trigger, disproved on Linux (2026-08-12)

The chain above is real. The claim that a **transient `/proc` read failure**
sets it off is not. Two probes in a Linux container (colima, 4 CPU), both
negative:

| Probe | Conditions | Result |
|---|---|---|
| `/proc/<pid>/status` read for a process that is **alive throughout** | 3× CPU oversubscription, continuous `fork`/`exec` churn of the process table | **300,000 reads — 0 errors, 0 partial reads** |
| Full ancestor walk where an **intermediate ancestor exits mid-walk** | same contention, 4,000 tree-churn iterations | **0 `Indeterminate`** — every walk ended `NotDescendant` |

The second result explains why the first refinement fails too: when a middle
process dies, the kernel **reparents its child to init immediately**, so the walk
finds `ppid <= 1` and terminates cleanly. There is no window in which the chain
"breaks under us" for a live leaf.

**So `linux::parent_of`'s conflation of "gone" with "unreadable" is real in the
code and, on this evidence, unreachable in practice.** It remains worth tidying
on its own merits — see Suggested fix — but it is **not** what makes this test
flake, and nothing here should be cited as if it were.

### Severity returned to medium

PR #109 raised this to high on the strength of a production story — a legitimate
client silently denied its resume — that depended entirely on the trigger just
refuted. With no established trigger there is no evidence of production impact,
so the rating goes back to **medium**, carried by the original argument: this is
the suite that stands as evidence for REQ-569's ancestry gate and REQ-570's
attestation split, and a suite that reddens intermittently teaches people to
re-run it.

### What is established, and what is not

**Established** — the withholding chain, verified link by link in source:
`None` → `Indeterminate` → `may_hold_session_access()` false →
`may_answer: false` → `deliver()` skips the surface → the frame reaches nobody.
If anything ever yields `Indeterminate` for a legitimate client, a silent
20s/30s denial follows with no diagnosis available to the user. That property is
worth fixing regardless of this bug.

**Not established** — anything that produces `Indeterminate` here. Ruled out so
far: the test's barrier ordering (refuted by the disconnect sequence), both
routing arms, the creator's attachment, `MAX_ANCESTRY_DEPTH` exhaustion (64, and
real chains are a handful deep), `SO_PEERCRED` failure on a connected socket,
transient `/proc` read failure, and ancestor-exits-mid-walk.

**Not reproduced anywhere outside CI.** macOS: 0 failures in 30 quiet runs of the
test and 12 runs of the full binary under full CPU saturation.

### A note on how this report went wrong twice

Both refuted root causes were arrived at the same way: read the code, find a path
that *could* produce the symptom, and mistake its tidiness for evidence. The
second was more persuasive than the first precisely because it explained the
platform asymmetry — and it was equally wrong. A mechanism that could produce a
symptom is not evidence that it did. The next step is deliberately an
*observation*, not a third hypothesis.

## Suggested fix

There is no fix to make yet — the cause is unknown. What follows is what should
**not** be done, and what is worth doing anyway.

**Do not raise `READ_DEADLINE`**, and do not touch the test's barrier. The
barrier is correct (proved above) and the deadline is doing its job by making a
withheld frame loud rather than slow. Raising it converts an occasional loud
failure into a slower occasional loud failure.

**Do not "fix" `linux::parent_of` as though it were the cause.** Distinguishing
`ENOENT` from a transient read error is defensible tidying — the current `.ok()?`
does conflate two different facts — but the probes say that conflation is
unreachable in practice, so doing it would close this bug in appearance only.
If it is done, it must be on its own merits and with `Indeterminate` still
terminal: **retry then `Indeterminate`, never retry then assume.** Treating a
still-alive but unreadable process as eligible would convert a spurious denial
into a spurious admission, straight through REQ-569's ancestry gate.

**Worth doing regardless of this bug:** an `Indeterminate` classification is
currently invisible. A legitimate client that trips it is refused with nothing a
user or operator can act on, and the daemon keeps no record of the decision. That
is a real gap in a security-relevant path independent of whether it is what makes
this test flake — and it is what the "observe, do not hypothesise" step above
addresses.


## FIRST INSTRUMENTED CAPTURE — 2026-08-12

The instrument fired on its first opportunity, on CI for PR #116 (a **docs-only**
`.adlc` change), ubuntu leg. This is the first evidence anyone has had about this
bug, and it **excludes two of the three candidate mechanisms outright.**

Captured daemon log, surfaced by #112's panic dump:

```
teton-code: client_connected    (live_connection_count=1)   <- watcher
teton-code: client_connected    (live_connection_count=2)   <- owner
teton-code: client_connected    (live_connection_count=3)   <- peer
teton-code: client_disconnected (live_connection_count=2)
teton-code: client_disconnected (live_connection_count=1)
teton-code: client_connected    (live_connection_count=2)   <- resumer
tetond: Cli client "resume-client" approved its own attach consent — no other
        client was attached to that session, so the prompt was rendered at the
        connection that asked for it (REQ-569 BR-6 second arm)
```

### What this rules out

- **The ancestry seam is CLEARED.** #111 logs a line for every classification
  that is not `NotDescendant`. There is none. So no connection in this run was
  `Descendant` or `Indeterminate`, and the mechanism #109 published as "the
  actual root cause" is now positively **excluded**, not merely unproven.
- **"The prompt reached nobody" is CLEARED.** #113 logs whenever a consent frame
  is delivered to zero surfaces. There is none. Every prompt reached a surface.

### What this localises

**Leg 2 succeeded.** The self-approval line is the daemon's record of the
*resume* consent being granted — the thing the test asserts last, and the whole
point of the arm under test. So consent was sought, delivered, answered, and the
grant minted.

The read that times out therefore comes **after** that: the test's
`read_response(resume_attach)` loop, waiting for the `session/attach` RPC
response to arrive on the resumer's socket. `read_response` loops on
`read_frame` until it sees a frame whose `id` matches; the deadline fires inside
that loop.

So the shape is: **the daemon completed the attach and the response frame never
reached the client** — or reached it in a form the id match did not recognise.
That is a delivery/ordering question one layer downstream of consent, and it is
territory none of the three refuted hypotheses touched.

### Not a fourth hypothesis

One sample. The above is what the log *shows*, plus the narrowing that follows
from two absent lines — no mechanism is being proposed here. This report has
already published two confident wrong causes, and the discipline that produced
this evidence was refusing to guess a third time.

What would settle it: the resumer's frame stream at the moment of the timeout.
`read_frame` records event notifications as it passes them, so dumping the
frames a client *did* receive when its deadline fires is the natural companion
to #112 — the daemon's side is now visible and the client's still is not.

Worth noting for whoever picks this up: REQ-570 added a daemon-wide
`grant_minted` broadcast to every handshaked connection (AC-9), so this arm now
produces event traffic on connections that are not party to the attach, and
`ConsentSurfaces::deliver` uses a non-blocking `try_send` whose failure is
silent. That is an observation about what changed near this seam, not a claim
that it is the cause.

## Next step: observe, do not hypothesise

Two hypotheses have now been refuted, and a third would be worth less than one
measurement. **Make the next occurrence diagnosable**, then wait for it.

The daemon computes each connection's `Ancestry` exactly once, at handshake, and
then discards the inputs. When CI next goes red there is no way to tell whether
`Indeterminate` was involved at all — which is why this has cost two rounds of
guessing. Recording the classification and the peer pid at handshake turns the
next failure into a single log line that either implicates this mechanism or
clears it for good.

That is a small, additive change to a security-critical path: it records a
decision already being made and changes no predicate. It is the cheapest thing
that can end this.

## SECOND INSTRUMENT ADDED — 2026-08-22 (still open, deliberately)

Asked to fix every open bug, this one was **not** fixed, because its own
evidence says a fix would be a guess. The report already published two
confident wrong causes, and the first instrumented capture *positively
excluded* the ancestry mechanism rather than merely failing to confirm it.
Patching `linux::parent_of` — the obvious-looking change, and the one the
"withholding chain" section still describes — would have been the third wrong
cause. The prescribed next step was a measurement, so a measurement is what was
added.

**What landed** (`crates/tetond/tests/attach_authorization.rs`): the client half
of the dump the last capture asked for. `RawClient` now records a one-line
structural summary of **every** frame it reads — responses with their ids, not
only event notifications — plus the response id `read_response` is currently
blocked on. The deadline panic prints both.

This is precisely the distinction the last capture left open. The flake was
localised to `read_response`'s loop *after* consent was granted, leaving two
shapes: the response never arrived, or it arrived in a form the `id` match did
not recognise. `events` could not tell them apart; this can.

The summary is **structural only** — method, id, event name, error code, never
the payload. A consent frame carries file-authored text and a panic message is
not a place to reproduce it. It is unit-pinned
(`the_frame_summary_distinguishes_the_shapes_bug_163_has_to_tell_apart`),
because an instrument that renders garbage at the moment everything else failed
is worse than none.

No predicate changed. Status stays `open`: the next red CI run should now
identify or clear the delivery/ordering seam in one read.

## Related

- REQ-569 ADR-A/ADR-B — the ancestry gate and its per-platform peer-PID split;
  ADR-A-2 records the previous conflation of "attached" with "eligible to answer"
- REQ-570 — introduced the test that surfaced this; its BR-3/BR-5 own the
  self-render arm
- **BUG-159 / [[LESSON-489]]** — the same shape one layer up: a read failure
  recorded as a fact about the world. Fixed there for files; this is the process
  version, and the two were found four hours apart
- [[LESSON-502]] — an invariant enforced at several seams needs a test at each
  seam; this is the seam's test being unreliable
- [[LESSON-433]] — cfg-gated platform code verified on one OS is false
  confidence. `peer.rs` cites it and splits the *parser* out to be testable
  everywhere; the **read** is the part that stayed platform-only, and the read is
  where the defect is

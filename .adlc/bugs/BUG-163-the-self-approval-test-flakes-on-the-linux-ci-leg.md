---
id: BUG-163
title: "A transient /proc read is treated as a vanished process, so a legitimate client is silently denied consent (surfaced as a flaky attach test on Linux)"
status: open
severity: high
created: 2026-08-12
updated: 2026-08-12
component: "daemon/peer"
domain: "session-authorization"
stack: ["rust", "daemon", "json-rpc", "linux"]
concerns: ["security", "availability", "test-determinism"]
tags: ["ancestry-gate", "procfs", "attach-consent", "may-answer", "flaky-test", "linux", "REQ-570", "REQ-569"]
found_by: "CI on PR #107 (a docs-only change), 2026-08-12"
introduced_by: REQ-569
---

## Description

> **The title and severity changed after analysis (2026-08-12).** This was filed
> as a flaky test at medium. It is a **daemon defect** at high: on Linux, a
> transient `/proc/<pid>/status` read failure is indistinguishable from a
> vanished process, and the daemon silently denies the affected connection the
> right to answer an attach-consent prompt. The flaky test is the symptom that
> made it visible. The filename still carries the original slug so links from
> PR #108 keep resolving.

**The defect.** `linux::parent_of` treats any `/proc` read error as "process
gone", which becomes `Ancestry::Indeterminate`, which sets `may_answer: false` on
that connection's consent surface, which makes `deliver()` skip it. The consent
frame is then sent to nobody and the request waits out its window. In production
that is a legitimate user's resume flow failing at 30 seconds with no
explanation; in CI it is a 20-second read deadline and a red suite.

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
- Failing run: <https://github.com/atelier-fashion/teton-code/actions/runs/31598675582/job/94120352305>

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

### The actual mechanism

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

### Why this explains what was observed, where the first hypothesis did not

- **Linux-only** — macOS uses `sysctl(KERN_PROC_PID)`, which has no file read to
  fail. The first hypothesis was platform-neutral and could not account for the
  asymmetry.
- **Load-sensitive** — `/proc` reads get less reliable exactly when the runner is
  contended, which is when it fired.
- **~1 in 9** — a transient error rate, not a logic error.
- **Twenty seconds of total silence** — the frame is *withheld*, not delayed.

### Not reproduced

Still a hypothesis, however clean the chain. macOS gave **0 failures in 30 quiet
runs** of the test and **12 runs of the full binary under full CPU saturation** —
consistent with a Linux-only mechanism, but not evidence for it. A Linux
reproduction is the next step and should land before the authorization path is
touched.

## Why this is a product defect, not only a flaky test

The severity was raised from medium to **high** here. The same transient failure
in production silently downgrades a **legitimate** client to "may not answer
consent", so a real user's resume flow dies at the 30-second window with no
explanation and nothing in the error naming the cause.

The direction is fail-**closed**, so this is not a hole in REQ-569's perimeter —
it is a spurious denial of a core flow. The rate on an ordinary developer machine
is unmeasured and probably well below CI's 1-in-9; if it turns out negligible in
practice this could come back down. It is rated on impact-when-it-fires, and on
the fact that the failure is silent and undiagnosable from the user's side.

The flaky test is the messenger.

## Suggested fix

**Do not raise `READ_DEADLINE`**, and do not touch the test's barrier — the
barrier is correct and the deadline is doing its job by making a withheld frame
loud.

Narrow the Linux arm so it distinguishes *gone* from *unreadable*: `ENOENT` (and
`ESRCH`) is a vanished process and stays `None`; any other error is a failed read
and should be retried before the walk gives up.

**The failure semantics are the security-sensitive part and must be chosen
deliberately.** `Indeterminate` must remain the terminal answer when the read
cannot be completed — retry then `Indeterminate`, never retry then *assume*.
Treating a still-alive but unreadable process as eligible would punch a hole
straight through REQ-569's ancestry gate, converting a spurious denial into a
spurious admission. Fail-closed is the correct direction; the bug is only that we
arrive there on evidence we do not actually have.

Worth doing at the same time: make an `Indeterminate` classification **observable**.
Today a legitimate client that trips this is refused with no signal a user or an
operator could act on, which is why this surfaced as a test timeout rather than
as a report.

## Next step (agreed 2026-08-12)

**Reproduce on Linux before touching the authorization path.** The chain above is
read off the code and explains every observed property, but it has not been
executed. A change to REQ-569's ancestry gate should rest on a reproduction, not
on reasoning however clean — that gate is the perimeter, and the fix's whole
subtlety is in its failure semantics.

Shape of the reproduction: build the workspace in a Linux container, run the
`attach_authorization` binary in a loop under CPU contention, and confirm both
that the failure appears and that it is preceded by an `Indeterminate`
classification. Injecting a fault at the `ParentOf` seam is the cheaper
confirmation — `is_descendant_of` already takes a `&dyn ParentOf`, so a lookup
that fails transiently can be simulated on any platform, and the daemon-level
consequence observed without procfs at all.

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

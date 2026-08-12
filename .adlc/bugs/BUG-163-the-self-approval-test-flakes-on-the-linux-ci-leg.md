---
id: BUG-163
title: "The self-approval attach test flakes on the Linux CI leg — the suite that guards attach authorization goes red for no reason"
status: open
severity: medium
created: 2026-08-12
updated: 2026-08-12
component: "daemon/session"
domain: "verification"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["security", "test-determinism"]
tags: ["flaky-test", "attach-consent", "self-approval", "race", "ci", "REQ-570", "REQ-569"]
found_by: "CI on PR #107 (a docs-only change), 2026-08-12"
introduced_by: REQ-570
---

## Description

`tetond/tests/attach_authorization.rs::a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log`
intermittently fails on the **ubuntu-latest** CI leg by timing out after 20
seconds waiting for a frame from the daemon.

It is filed at medium rather than low because of *which* suite it is in. The
attach-authorization tests are the standing evidence for a security perimeter —
REQ-569's ancestry gate and REQ-570's attestation split. A suite that goes red
intermittently trains everyone to re-run it, and a real regression in that
perimeter then gets re-run away with the noise. The flake itself costs a CI
cycle; the habit it teaches costs the guarantee.

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

**Hypothesis, not established — this was observed once and has not been
reproduced under a debugger.** Recorded so the next person starts from evidence
rather than from scratch.

**"Slow runner" is the weak explanation and probably wrong.** The failure is a
20-second read timeout on a *local Unix socket*, and every other test in the
same binary completed in milliseconds on the same runner. Twenty seconds of
total silence is not a machine being busy; it is a daemon that sent nothing
because it is waiting for something.

**The likely mechanism is an ordering race in the test's own barrier.** Leg 2 of
the test does this:

```rust
owner.disconnect();
peer.disconnect();
watcher.wait_for_client_count(1);   // the ordering marker
let mut resumer = RawClient::connect(daemon.socket());
let resume_attach = resumer.send("session/attach", …);
```

The intent is that by the time `resumer` attaches, nobody holds the session, so
the consent prompt takes REQ-569 BR-6's **self-render** arm and the requester
answers its own prompt — which is what the test asserts the daemon logs.

But `wait_for_client_count(1)` synchronizes on the daemon's **client count**,
which is not the same fact as the **session's consent-surface registry** having
dropped the departed connections. REQ-569 ADR-A-2 records that those two were
conflated once already, in the other direction: consent surfaces were registered
only for connections that passed the ancestry gate, so a session someone held
looked *unheld*. If the client count can reach 1 before the registry is
cleaned, then `resumer`'s attach takes arm 1 (`ConnectionsAttachedTo`) instead,
routes the prompt to a **departed** connection, nobody answers, and the read
blocks until the deadline — producing exactly this sentence, which the test
author wrote for exactly this situation.

That would make the barrier a **proxy for the condition it means to establish**,
which is the same shape as LESSON-443 one level out: the guard names something
adjacent to what it guards.

## Suggested fix

Do **not** raise `READ_DEADLINE`. The deadline is correctly generous, and
lengthening it would convert an occasional loud failure into a slower occasional
loud failure while hiding the ordering question.

Establish the fact the test actually depends on: wait until the **session has no
attached consent surfaces**, not until the daemon's client count drops. If the
daemon exposes no such observable, that absence is itself worth knowing — the
self-render arm's precondition would then be unobservable from outside, which
makes it hard to test deliberately and easy to hit accidentally.

Worth checking while there: whether a departed connection can still be selected
as a consent surface in production, not just in this test. If it can, this is not
only a flaky test — it is a consent prompt that can be routed into the void, and
the 30-second shipped consent window would be the only thing ending it.

## Related

- REQ-570 — introduced this test; its BR-3/BR-5 own the self-render arm
- REQ-569 ADR-A-2 — the previous conflation of "attached" with "eligible to
  answer", in the opposite direction
- [[LESSON-502]] — an invariant enforced at several seams needs a test at each
  seam; this is the seam's test being unreliable
- BUG-159 — the other flake that made this repo's verification occasionally lie;
  different mechanism, same cost

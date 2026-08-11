---
id: LESSON-505
title: "An audit control is judged in the adversarial case, not the honest one — and 'we log it' is only as strong as the log"
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security", "reliability"]
tags: ["observability", "audit", "accepted-residual", "logging", "self-approval", "req-569"]
req: REQ-569
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

REQ-569 accepted a residual: when nobody is attached to a session, the
connection *requesting* attach renders its own consent prompt, so a headless
same-UID process can approve itself. The mitigation was "accepted, but never
silent" — the daemon logs a self-approval line.

Review dismantled that in two directions.

**The log was the wrong home.** Daemon stderr is read by the CLI only when the
daemon fails to start, is truncated at 256 KiB on the next spawn, and — being a
file owned by the same uid — is writable by the very adversary whose
self-approval it records. "Recorded in a place nobody reads and the attacker
can erase" is not "announced." The fix was to publish a daemon-scoped *event*
instead, which the existing delivery rules push to every connected client, in
front of a live human, where the adversary cannot suppress it.

**Then the replacement had the same blind spot as the bug it was watching for.**
The new event carried a `self_approved` flag computed from the *route arm*. An
attacker holding two connections — X approving Y — is not one connection
approving itself, so the flag read `false` and the notice rendered as benign.
That is verbatim the reason the monitor consent path had been deleted days
earlier ("two different connection ids, so it did not even register as a
self-approval"). The control built to make the residual visible was blind in
precisely the case it existed for, and it was written by the same reasoning
that had just condemned that blindness elsewhere.

## Lesson

Two rules, and the second is the one that keeps getting missed.

1. **An audit control inherits the threat model of what it audits.** If the
   adversary can truncate, suppress, out-shout, or simply outlive the record,
   the residual is unmitigated regardless of how faithfully the honest path
   logs. Ask where the record lands, who can write there, who ever reads it, and
   what happens when it is produced in a loop.
2. **Evaluate the control against the adversary, not the happy path.** A flag,
   metric, or alert is worth exactly what it reports when someone is attacking
   — and a field derived from *internal shape* (which code arm ran) rather than
   from the *relation you care about* (did the same actor ask and answer) will
   read clean under an attack that routes around the shape. Derive audit fields
   from the property, then test the field with an adversarial fixture, not only
   an honest one.

## Why It Matters

Both failures shipped inside remediation work — the second was introduced *by
the fix for the first*, in the same session, by someone who had just written
down why that blindness was dangerous. Knowing the pattern is not the same as
checking for it in your own diff, which is the argument for adversarial review
of fixes and not only of features (this one was caught by a second review pass
with live probes, after the first pass's Critical was closed).

Accepting a residual is legitimate; REQ-569 accepted several. What is not
legitimate is resting the acceptance on a mitigation nobody checked from the
attacker's side. Related: [[LESSON-504]] (the precondition flaw this control
was built to observe), [[LESSON-502]] (a passing suite proves only the seams you
wrote tests for).

## Applies When

Adding logging, events, metrics, or alerts as the compensating control for an
accepted risk; writing "this is safe because it is observable" in a spec or
ADR; reviewing any boolean whose name asserts a security property — check what
it reports when an attacker takes a path the implementation did not enumerate.

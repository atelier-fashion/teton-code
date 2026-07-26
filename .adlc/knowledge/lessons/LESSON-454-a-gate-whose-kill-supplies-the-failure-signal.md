---
id: LESSON-454
title: "When a watchdog's kill supplies the failure signal, the gate passes the violation it exists to catch"
component: "distribution/release"
domain: "testing"
stack: ["bash", "ci", "github-actions"]
concerns: ["reliability", "testing", "security"]
tags: ["watchdog", "timeout", "fail-open", "gate", "smoke-test", "assertion-provenance", "br-9"]
req: REQ-548
created: 2026-07-26
updated: 2026-07-26
---

## What Happened

The release smoke gate asserts that a shipped `tetond` refuses to start under
`TETON_TEST_SEAMS=1` — the one mechanical check that release binaries cannot be
steered by test seams. It was written as: run the daemon under a watchdog that
`kill -9`s it after N seconds, then require `exit != 0` **and** the refusal text
in the output.

A daemon that printed the refusal line and then kept running — honouring the
seams, i.e. exactly the violation the assertion exists to catch — satisfied
both halves. The watchdog's own kill manufactured the non-zero exit; the text
was there because the daemon had printed it before carrying on. The gate scored
it PASS. The header comment reasoned carefully about the *converse* hole ("a
non-zero exit alone would also be satisfied by a daemon that died for an
unrelated reason") and never noticed that the harness was supplying the very
signal being tested.

Nothing caught it until a selftest was written that fed the gate a deliberately
seams-honouring stand-in and demanded it go red.

## Lesson

Ask of every assertion: **who produced the evidence?** If any part of the pass
condition can be manufactured by the test harness — a timeout kill, a fixture's
default, a retry that masks the first failure — the assertion is measuring the
harness, not the subject. The fix is to make the harness's intervention
*visible to the assertion*: record that the watchdog fired (a marker file, a
flag) and treat "we had to kill it" as a distinct FAIL, never as evidence of
the exit code you wanted.

Corollary: a gate is only a gate if a known-bad input makes it go red, and the
only way to know is to build the known-bad input and watch. Every gate that
guards a release deserves a test that feeds it a violation.

## Why It Matters

This is fail-open in the one place fail-open is worst: a gate that certifies
bytes about to be installed on users' machines. It is invisible to review (the
condition reads as strictly stronger than a bare exit check), invisible to
green CI (the gate passes on good artifacts too), and only observable by
deliberately supplying the failure it is meant to catch.

## Applies When

- Writing any timeout/watchdog around a process whose exit status is part of an
  assertion (smoke tests, health checks, integration harnesses).
- Reviewing a compound assertion — check each conjunct's provenance separately;
  `A && B` is not stronger than `A` if the harness supplies `A`.
- Any gate that guards a release, a deploy, or a security property: pair it
  with a known-bad fixture (see [[LESSON-451]] — a fake that bypasses the real
  path proves the fake works, not the path).

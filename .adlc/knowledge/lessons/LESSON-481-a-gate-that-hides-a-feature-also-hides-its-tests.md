---
id: LESSON-481
title: "A gate that hides a feature from users also hides it from the test suite — split the logic out from under the gate"
component: "cli"
domain: "clients"
stack: ["rust", "cli"]
concerns: ["test-coverage", "developer-experience", "reliability"]
tags: ["tty-gating", "pure-state-machine", "test-blindfold", "pty", "fixture-isolation", "req-556"]
created: 2026-08-04
updated: 2026-08-04
---

## What Happened

REQ-556 added a loading indicator to the interactive CLI. BR-2 gated it on
stdout being a terminal, so piped output would stay byte-identical — a
requirement inherited from REQ-549 and pinned by `cli_e2e`'s whole-output
equality tests.

That gate is correct, and it made the entire feature **invisible to every test
in the suite**. `cli_e2e` drives `teton` over pipes; under a pipe the indicator
emits nothing. Written the obvious way — computing frames inside the render
path — REQ-556's core behaviour would have shipped with no automated coverage
at all, and the gate would have been the reason.

The spec's first draft did not notice. Validation caught it as a blocker: five
of eight acceptance criteria had no stated way to be demonstrated.

## Lesson

**When a feature is gated on an environment the test suite does not provide,
the gate is also a test blindfold.** Ask early — at spec time, not at
implementation time — "which harness can observe this?" If the answer is
"none", that is a finding about the design, not a detail to sort out later.

Two remedies, and the REQ used both:

1. **Split the logic out from under the gate.** The indicator became a pure
   state machine: `(observed stages, tick) → Option<String>`, no I/O, no
   terminal, no clock. Everything the feature *decides* is then unit-testable
   with no gate in the way; only the few bytes that actually reach the terminal
   remain gated. Making the frame sequence a pure function was not an
   aesthetic choice — it was the only way the behaviour could be verified at
   all.
2. **Pay for the harness the gate demands, or record the gap.** A pty
   dev-dependency bought the timing claim ("an event renders while the session
   is idle"). It did *not* buy the animation, which needs the daemon parked in
   a load window no seam can hold. That gap is written into the task, the PR,
   and the manual-verification checklist rather than quietly dropped.

## Why It Matters

A gate that suppresses output for a good reason (byte-identical pipes, quiet
CI, non-interactive scripts) is exactly the kind of thing nobody re-examines,
because it is *right*. The coverage hole it opens is silent: the suite is
green, the feature is real, and no test touches it. The failure mode is a
regression shipping months later with every check passing.

The cheap tell is at spec time. If an acceptance criterion cannot name the
harness that observes it, either the logic moves out from under the gate or the
harness gets built — and if neither happens, the criterion is aspirational and
should say so.

## Corollary — a fixture that does not assert its own isolation can pass for the wrong reason

The pty test in this REQ passed on its first run *while silently attached to
the developer's real daemon* rather than its own fixture. It went green for a
reason unrelated to the code under test, and only failed later when that
daemon was busy. A green test that is not testing what it claims is worse than
a red one, because it stops anyone looking.

The fix is one assertion: the fixture pins a distinctive value (`16 GiB` of
probe RAM) and the test checks for it **before** trusting anything else it
sees. Any harness that spawns its own service against a shared discovery path
— a socket, a port, a well-known directory — wants that assertion.

(The related trap underneath it: `TETON_TEST_SEAMS=1` is required or the probe
seams are *ignored*, so the fixture daemon silently probed the real machine.
A seam that no-ops when its master switch is unset will look like a broken
test rather than a missing env var.)

## Applies When

- Adding behaviour behind a TTY / interactive / platform / feature-flag gate.
- Writing an acceptance criterion for anything the default harness cannot see.
- Standing up a test fixture that spawns a service reachable by the same
  discovery mechanism a developer's real one uses.

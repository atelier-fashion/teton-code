---
id: LESSON-554
title: "When two hypotheses have failed, instrument the observer — the bug may be in the thing doing the looking"
component: "tests/harness"
domain: "verification"
stack: ["rust", "ci"]
concerns: ["test-determinism", "observability", "debugging-method"]
tags: ["flaky-test", "instrumentation", "read-response", "out-of-order", "in-flight-requests", "linux-ci", "bug-163", "refuted-hypothesis"]
req: BUG-163
created: 2026-08-22
updated: 2026-08-22
---

## What Happened

A self-approval attach test flaked on the Linux CI leg for ten days. Its report
published **two confident wrong causes** — a barrier ordering race, then an
ancestry-seam withholding chain — and refuted both. A first round of
instrumentation *positively excluded* the second and localised the failure to
`read_response`'s loop, after consent had already been granted.

Every hypothesis had been about the **daemon**. The bug was in the **test
client**.

`RawClient` is single-threaded and deliberately keeps two requests in flight:
`session/attach` is sent and not awaited (it cannot answer until the consent it
raises is decided), then `permission/respond` is sent and read. `read_response`
looped until it saw its own id and **discarded every other frame**. When the
daemon answered the attach first, that response was consumed inside the
respond's read and thrown away — so the later read for the attach waited out its
full deadline for a frame that had already arrived.

Which of the two lands first is a race. macOS usually won it; the slower ubuntu
runner lost it.

The capture that settled it was one line of a dump:

```
awaiting response id 2
received 7 frame(s):
    ...
    5. response id=2 ok        <-- the frame it timed out waiting for
```

It fired on the *first* CI run after the instrument landed.

## Lesson

**When a flake survives two refuted hypotheses, stop hypothesising and
instrument the observer.** The daemon's side had been logged for two rounds; the
client's had not, and the client was where the bug was. A test harness is
production code for the purposes of debugging — it can be the defect, and it is
the one component nobody thinks to suspect because it is what you are looking
*through*.

Three specifics worth carrying:

1. **Record identity, not just occurrence.** The earlier instrument logged event
   *notifications*. This one logged **responses with their ids**, and that is the
   only reason "the frame never arrived" could be told from "the frame arrived
   and was discarded". A log that cannot distinguish your remaining hypotheses
   is not yet an instrument.
2. **Any reader that can have more than one request in flight must buffer
   responses it is not waiting for.** Discarding non-matching frames is correct
   only for a strictly request/response client, and a client that sends a second
   request before reading the first's reply is not one — even when it is
   single-threaded.
3. **Reproduce by construction, not by timing.** The fix's test sends two
   requests and reads the *second* first, so the interleaving is forced rather
   than raced. A test that waits for a flake to recur is not a regression guard.

## Why It Matters

This cost two rounds of confident wrong analysis and ten days of a red-on-docs-
only CI leg — the kind of failure that trains a team to re-run CI rather than
read it. The measurement that ended it was smaller than either of the fixes that
did not.

It also generalises past this repo: `read_response`-shaped helpers are in most
hand-rolled protocol test clients, and the "discard what I am not waiting for"
loop is the obvious first implementation of one.

## Applies When

- A test is intermittent, platform-skewed, and two mechanisms have already been
  proposed and refuted.
- Writing or reviewing a hand-rolled protocol client for tests, especially one
  that sends a request before reading a previous reply.
- Deciding what a diagnostic should record: ask which of your live hypotheses
  the output would separate, and add whatever it takes to separate them.
- Tempted to fix a flake by retrying, widening a deadline, or patching the
  component the last theory blamed.

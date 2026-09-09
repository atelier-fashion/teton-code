---
id: LESSON-656
title: "A library degradation path with no caller is a promise the spec keeps marking as kept"
component: "tetond/model_consent"
domain: "local-tier"
stack: ["rust"]
concerns: ["reliability", "traceability"]
tags: ["step-down", "br-8", "benchmark", "consent-gate", "dead-path", "test-seam", "bug-219", "req-544", "req-547"]
req: BUG-219
created: 2026-09-09
updated: 2026-09-09
---

## What Happened

REQ-544 shipped `benchmark_with_step_down` — the walk from a model that misses
the BR-8 latency duty to the next smaller catalog entry — with unit tests, and
its requirement listed auto-step-down among the things that were done. REQ-547
then moved loading and benchmarking behind the consent gate, and the gate's
duty-failure arm was written to the one-model shape the gate knew: abandon the
engine, publish `disabled`, return `EngineLoadFailed`. Nothing in the daemon
ever called the walk. REQ-547's own description inherited "a post-download
micro-benchmark with auto-step-down" from REQ-544 as an existing fact, and the
only producer of the `stepped_down` lifecycle stage was the `TETON_FORCE_BENCH`
simulation in `runtime/engine.rs`, so the CLI rendered a stage no production
path emitted. On 2026-09-09 a 48 GiB machine's probe pick missed the duty
(2021 ms first token), the tier was disabled for the daemon's lifetime, and the
7B that would have passed was never tried.

## Lesson

When a new authority is put in front of an existing library flow, trace every
failure arm of the authority to the library's degradation path — or decline it
explicitly in a comment. A spec's description that inherits a capability from
its predecessor is not evidence the capability is wired; `grep` for the caller.
A test seam that fabricates a lifecycle event nothing else produces is a
second kind of false evidence: the UI, the docs, and the reviewer all see the
event exist.

## Why It Matters

The failure mode is silent and total: the tier that the whole local-first
design rests on vanishes on exactly the machines where the largest model was
proposed, and the error the user sees points somewhere else. It cost a
three-layer diagnosis to find, against a fix of forty lines.

## Applies When

Wrapping a library flow in a gate or authority; reviewing a REQ whose
description says a feature "exists" from an earlier REQ; seeing a `Forced` or
simulated seam that is the only producer of an event; any `DutyOutcome`,
health check, or budget whose failure arm ends in a terminal state.

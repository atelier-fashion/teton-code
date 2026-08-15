---
id: LESSON-530
title: "A copied fixture must keep its synchronization barriers — and a timeout window must bound only the work it names"
component: "tests/pty_e2e"
domain: "testing"
stack: ["rust"]
concerns: ["flakiness", "race-conditions", "test-fixtures"]
tags: ["readiness-barrier", "timeout-window", "fixture-copy", "autostart-race", "lesson-450", "bug-164"]
req: BUG-173
created: 2026-08-15
updated: 2026-08-15
---

## What Happened

`pty_e2e.rs`'s `TestDaemon` was documented as a copy of `cli_e2e`'s fixture
shape but silently dropped that fixture's `wait_for_socket()` readiness
barrier. The suite's fixed 20s `WINDOW` — designed to bound a *client-side*
wait for the entry prompt — therefore had to absorb daemon process spawn and
full runtime assembly as well, concurrently with three sibling tests. On a
degraded ubuntu runner the sum crossed 20s with every process behaving
correctly (BUG-173: banner-only transcript, green on re-run). The missing
barrier also left a latent, meaner race: a client reaching a not-yet-bound
socket walks `teton`'s autostart path and spawns a second daemon with none of
the fixture's seams, racing it for the single-instance flock — the exact
signature BUG-164's resolution records.

## Lesson

When copying a fixture's shape, its **synchronization topology is the
load-bearing part** — the barriers, not the struct fields. And a timeout
window must bound only the work it names: if it guards "entry prompt
reached", the clock must start after every prerequisite (daemon readiness) is
*proven*, with a connect — not an existence check — as the proof. Restoring
the barrier both removed foreign startup from every wait and made the
autostart race structurally unreachable; widening the window (20s → 60s) was
then safe because state-reached waits (LESSON-450) charge the ceiling only to
runs that are already failing.

## Why It Matters

A window that absorbs someone else's startup is a statistical time bomb: it
passes locally, flakes on shared runners, and teaches people to re-run
instead of investigate. The drift is silent precisely because the copy
"looks the same" — the omission is invisible until resource pressure makes it
visible as an unrelated test's failure.

## Applies When

Copying or refactoring test fixtures across suites; setting deadlines for
"ready" states that depend on another process's startup; any harness where
the client has an autostart fallback that bypasses test seams; reviewing a
fixture documented as equivalent to another — verify the barriers came along.

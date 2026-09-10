---
id: BUG-226
title: "A running tool parks the worker its event forwarder is queued on, so `[running]` reaches the client with `[done]`"
status: resolved
severity: medium
created: 2026-09-10
updated: 2026-09-10
component: "tetond/harness/turn_loop"
domain: "session-ui"
stack: ["rust", "daemon", "tokio"]
concerns: ["developer-experience", "performance"]
tags: ["tool-dispatch", "block-in-place", "lifo-slot", "event-forwarder", "activity-row", "flaky-test", "req-621", "req-544"]
introduced_by: ["REQ-544"]
attribution: manual
---

## Description

`Tool::run` is synchronous and `run_the_allowed_tool` dispatched it inline on
the tokio worker running the turn, so a `shell` call held that worker for as
long as its child ran — up to the tool's timeout ceiling. Immediately before
the dispatch, `tool_started` published `tool_call` to the bus, whose `try_send`
woke this connection's event forwarder. A task woken from inside a worker is
placed in that worker's LIFO slot, and the LIFO slot cannot be stolen by other
workers (tokio-rs/tokio#4941). When the turn task did not yield between the
publish and the dispatch, the forwarder sat in the slot until the tool
returned, and the client received `tool_call` and `tool_call_update` back to
back: the `[running]` line printed at the moment the tool finished, with the
`[done]` line on its heels.

REQ-621 made this observable. Its activity row shows a running tool's title and
elapsed seconds beneath the `[running]` line, driven by the daemon's own
publisher on both edges (AC-5), and its pty leg
`a_running_tool_shows_its_title_elapsed_and_cost_so_far_beneath_its_running_line`
asserts that the counter shows at least two distinct values while a `sleep 3`
runs. On 2026-09-10 that leg failed once in a full run and passed on every
re-run: a compressed window leaves exactly one row, painted by the `tool_call`
arm and withdrawn by the next message.

## Reproduction Steps

1. Run `cargo test -p teton --test pty_e2e` repeatedly under load; the leg
   above fails intermittently with one row where it expects a moving counter.
2. Deterministically, on tokio 1.53: a task that `try_send`s to a parked
   receiver task and then blocks its thread for 1.5 s leaves the receiver
   waiting the full 1.5 s in 8 of 8 trials; the same block inside
   `tokio::task::block_in_place` leaves it waiting 0 ms in 8 of 8.

## Expected Behavior

The `[running]` line and the activity row appear when the tool starts, and the
row's counter advances while it runs, whatever the tool's length.

## Actual Behavior

Intermittently, `[running]` and `[done]` arrive together when the tool ends,
and the stretch while it ran is silent — the defect REQ-621 exists to remove.

## Environment

- Platform: macOS 25.6, tokio 1.53.0 multi-thread runtime
- Version: teton-code v0.1.34 with REQ-621 in flight

## Root Cause

REQ-544 shaped `Tool::run` as synchronous ("tool work is filesystem and process
I/O, and the loop dispatches it inline") and REQ-600's decomposition carried
the inline `tools.dispatch(name, tool_ctx, arguments)` into
`run_the_allowed_tool`. The turn path's blocking-wait check
(`the_turn_path_takes_no_blocking_wait`) scans for filesystem idioms and never
listed the dispatch, which is spelled like any other method call. The skill
tool already routed its own shell runs to the blocking pool for exactly this
reason; the `shell` tool's path did not.

## Resolution

`run_the_allowed_tool` dispatches through
`crate::runtime::block_in_place_if_multithread`, which hands the worker's core
— run queue and LIFO slot — to a fresh thread before this one blocks. The pin
test records the call as the one argued blocking call in
`harness/turn_loop.rs` and adds a hazard-keyed check: every `tools.dispatch(`
in the production half must be the wrapped spelling, and there must be at
least one. The pty leg's doc comment carries the diagnosis and its claim (2)
message lists the rows in the tool's window so a one-row window and a frozen
counter are told apart on the spot. REQ-621's own re-verify had already made
claim (2) a polled condition; that stays, but polling cannot reopen a window
that closed with the tool, so neither the tool's length nor the `>= 2` claim
was loosened — a starved forwarder compresses the window to nothing however
long the tool runs.

## Deployment

- Pending merge.

## Files Changed

- `crates/tetond/src/harness/turn_loop.rs` — the dispatch behind the blocking helper
- `crates/tetond/src/runtime/mod.rs` — the argued call and the hazard-keyed dispatch check
- `crates/tetond/src/harness/tools/mod.rs` — the trait's contract note
- `crates/teton/tests/pty_e2e.rs` — the leg's diagnosis and claim (2)'s message

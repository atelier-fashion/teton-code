---
id: BUG-152
title: "A prompt typed while the local tier is still loading is reported as an error, not as a wait"
status: resolved
severity: medium
created: 2026-08-04
updated: 2026-08-04
component: "cli/session"
domain: "harness"
stack: ["rust", "cli", "daemon", "json-rpc"]
concerns: ["developer-experience", "first-run"]
tags: ["first-run", "error-message", "loading-window", "local-tier", "error-code"]
---

## Description

BUG-146 fixed *what* the daemon says when nothing can serve a turn: the
loading-window refusal now names the real state instead of blaming the local
engine. It did not change *how the client presents it*. Every refusal — a
withheld tier, a declined tier, a machine below the RAM floor, and a tier that
is simply thirty seconds from being ready — renders identically:

```
error: prompt failed: qwen3-coder-30b-a3b's weights are installed and verified;
the daemon is loading and benchmarking them now — the local tier opens when that
completes.  Retry in a moment. No remote provider is configured either — …
```

The sentence is correct and the presentation contradicts it. The word `error:`
is the first thing on the line, and it is the only part most people read: on a
first run the user has just been told, one line earlier, that the tier is
loading — and the very next thing the session does is call that an error.
Nothing is broken, nothing needs fixing, and the state ends by itself.

## Reproduction Steps

1. Install and start the daemon on a machine with a large recorded model
   (`qwen3-coder-30b-a3b`, 18.6 GB).
2. Run `teton`; the banner reports *"local tier disabled: … the daemon is
   loading and benchmarking them now"*.
3. Type any prompt inside that ~44 s window, with no remote provider
   configured.

## Expected Behavior

A waiting notice, in the same class as the lifecycle lines it continues — the
tier is on its way, the session stays usable, and the user can retype the
prompt in a moment:

```
>> model still loading — qwen3-coder-30b-a3b's weights are installed and
verified; the daemon is loading and benchmarking them now — … Retry in a moment.
No remote provider is configured either — …
```

## Actual Behavior

An `error:` line, indistinguishable from a declined tier, a failed load, or a
machine that will never have a local tier at all.

## Environment

- Platform: macOS 26 / Apple Silicon (M5 Max, 48 GiB)
- Version: teton 0.1.6

## Root Cause

The daemon distinguishes six unserved-turn states and reports all six with one
code — `UNKNOWN_PROVIDER` — because BUG-146 was about the *sentence*. The
client has one error arm for `prompt/turn`, so the only signal available to it
was the message text, and reading that text for keywords would be a second
classifier for one state: exactly the shape LESSON-456 warns about.

Two of the six states are transient (an install in flight, and verified weights
mid-load): they resolve with no user action at all. The other four — declined,
unanswered, below the floor, load failed — need an answer, a command, or
different hardware. That distinction existed in the classifier and was thrown
away at the wire.

## Resolution

- New `error_code::TIER_WARMING` (-32005). The one unserved-turn condition that
  ends by itself gets its own code; the four settled causes keep
  `UNKNOWN_PROVIDER`.
- `DaemonRuntime::unserved_turn_reason` becomes `unserved_turn_error` and
  returns the `RpcError` rather than a bare string, so the code is chosen in the
  same branch that chooses the sentence. A branch cannot get one right and the
  other wrong without failing its own test.
- The client's turn-failure arms move into `render_turn_failure`, which renders
  `TIER_WARMING` as a `Notice` under a leading headline (`model still loading
  —`) and everything else as the unchanged `prompt failed:` error line. The
  daemon's sentence is passed through whole — the headline is added in front of
  the reason, never in place of it.
- Extracting the arms is what makes them assertable without a socket, the same
  move `cost_report_or_report` made for the cost surfaces.

Deliberately unchanged: a failed load stays an error. Retrying it meets the same
dead engine, so rendering it as "still loading" would be a lie that costs the
user a wait.

## Files Changed

- `crates/teton-protocol/src/jsonrpc.rs` — `error_code::TIER_WARMING`
- `crates/tetond/src/runtime.rs` — `unserved_turn_error` (was
  `unserved_turn_reason`); per-branch codes; the classification tests now pin
  the code alongside the sentence; a doc comment that had been spliced onto
  `local_tier_available` restored
- `crates/teton/src/main.rs` — `render_turn_failure`, `TIER_WARMING_HEADLINE`,
  and a unit test covering both arms
- `crates/teton/tests/cli_e2e.rs` — `assert_no_turn_ran` learns the new marker,
  so the "no turn was attempted" guard stays load-bearing on this path too
- `crates/tetond/tests/e2e/consent_matrix.rs` — the starved-turn assertion pins
  the code as well as the message
- `docs/manual-verification.md` — the restart step checks the notice

## Lessons

- `LESSON-456` (applied, not amended) — one classifier per state, not one per
  surface. The fix here is the same rule one layer out: the split the daemon
  already knew about had to reach the wire, or the client would have had to
  re-derive it from prose.

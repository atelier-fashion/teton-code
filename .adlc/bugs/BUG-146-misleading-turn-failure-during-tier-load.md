---
id: BUG-146
title: "First prompt after install fails with a message blaming the local engine for a config/timing problem"
status: resolved
severity: high
created: 2026-07-31
updated: 2026-07-31
component: "daemon/router"
domain: "harness"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["developer-experience", "observability", "reliability"]
tags: ["first-run", "error-message", "misclassification", "local-tier", "loading-window", "no-provider"]
---

## Description

The first prompt a new user types after `brew install atelier-fashion/tap/teton`
fails with:

```
error: prompt failed: local engine could not serve the turn
```

The local engine is not at fault. On a fresh install there are no remote
providers, and on any machine with a large model the local tier spends ~44 s
deep-verifying, loading and benchmarking the weights before it opens. A turn
submitted inside that window has nothing to route to — but the message names
the one component that is working correctly and about to become available.

Two defects, one visible and one structural:

1. **The reason is discarded.** `Err(HarnessError::Engine(_))` in
   `run_prompt_turn` throws away the underlying `EngineError` and substitutes a
   fixed string, so *every* engine-class failure reports identically regardless
   of cause.
2. **A config/timing condition is classified as an engine failure.** The
   no-provider path builds `HarnessError::Engine(EngineError::unavailable("no
   provider for this turn"))` — a missing provider is not an engine error, and
   wrapping it as one is what makes defect 1 actively misleading rather than
   merely vague.

Separately, the daemon *knows* the tier is loading — it prints exactly that on
the lifecycle stream one line earlier — and that knowledge never reaches the
turn's failure path, where it is the actionable part.

## Reproduction Steps

1. Install on a machine with a recorded model decision and large weights:
   `brew install atelier-fashion/tap/teton`
2. `brew services start teton`
3. `teton` — observe the lifecycle line: *"local tier disabled: … the daemon is
   loading and benchmarking them now — the local tier opens when that
   completes."*
4. Immediately type any prompt (inside the ~44 s load window), with no remote
   providers configured.

## Expected Behavior

A message that names the real state and what to do about it — that the local
tier is still loading and will open shortly, and/or that no remote provider is
configured to serve the turn in the meantime. The session must stay usable
(BR-1/D-3: the gate withholds the tier, never the session).

## Actual Behavior

`error: prompt failed: local engine could not serve the turn` — blames the
local engine, offers no action, and is indistinguishable from a genuine engine
crash or a failed load.

## Environment

- Platform: macOS 26 / Apple Silicon (M5 Max, 48 GiB)
- Version: teton 0.1.0, installed from `atelier-fashion/tap` (the real
  Homebrew path, not a source build)
- Model: `qwen3-coder-30b-a3b` (18.6 GB), decision already recorded

## Root Cause

Three layers, each individually defensible, compounding into a message that
named the wrong subsystem.

1. **`build_router` invents a provider that does not exist.** With
   `config.providers` empty it falls back to the literal string `"local"` for
   *both* `default_provider` and `local_provider` (`runtime.rs:2430-2439`), and
   `route_freeform` never returns "no provider" by design
   (`heuristics.rs:89-91`). So a fresh install routes every freeform turn to a
   provider id that is registered nowhere — and emits a `route_decided` event
   announcing it.
2. **A config/timing condition was typed as an engine failure.** The
   unresolvable provider produced
   `HarnessError::Engine(EngineError::unavailable("no provider for this turn"))`
   (`runtime.rs:1306`). A missing provider is not an engine fault.
3. **The reason was then discarded.** `Err(HarnessError::Engine(_))`
   (`runtime.rs:1203`) dropped the inner error and substituted a fixed string,
   so every engine-class failure reported identically — including this one,
   which was not an engine failure at all.

The daemon had the correct classification the whole time: `startup_lifecycle`
already distinguishes loading / failed / declined / awaiting-consent and had
published "loading and benchmarking them now" one line earlier. That knowledge
simply never reached the turn's failure path.

## Resolution

- New `HarnessError::NoTierAvailable` — the "nothing could serve this turn"
  condition is now its own variant, not an `Engine` error. It deliberately
  carries no message: the actionable reason depends on daemon state the turn
  loop cannot see.
- New `DaemonRuntime::unserved_turn_reason` classifies that state using
  `startup_lifecycle`'s exact precedence (below-floor → declined → awaiting
  consent → load-failed → loading → no-loader), reusing the same BR-11-safe
  reason builders, so a turn failure and the lifecycle replay describing the
  same machine can never tell two different stories. Every branch appends what
  to do about it (`teton provider add`, or the routing check when a provider
  *is* configured).
- The genuine `Engine` arm now carries the engine's own sentence instead of
  discarding it. Deliberately NOT a blanket `e.to_string()` everywhere: the
  `without_path` scrubber lives at the loader call site rather than inside
  `EngineError`, so load-time errors could carry a weights path. Both reasons
  reachable on the turn path today are static literals.
- Error code for the no-tier case moves from `INTERNAL_ERROR` to
  `UNKNOWN_PROVIDER` — nothing internal went wrong.

Not fixed here, filed as a follow-up: `route_decided` still announces the
phantom `"local"` provider before the turn fails. That is router semantics and
wants its own change.

## Files Changed

- `crates/tetond/src/harness/turn_loop.rs` — `HarnessError::NoTierAvailable`
- `crates/tetond/src/runtime.rs` — both no-tier sites reclassified; the two
  `run_prompt_turn` arms; `unserved_turn_reason`; state-classification tests;
  stale doc reference
- `crates/tetond/tests/e2e/consent_matrix.rs` — the starved-turn assertion now
  checks the message names the cause, not merely that an error occurred

## Deployment

Not a deployed service — `teton`/`teton-code` ship as Homebrew binaries. The
fix landed on `main` (PR #11, squash `4b43976`) and **shipped in v0.1.1**
(2026-07-31), so `brew upgrade teton` now carries it. v0.1.0 still reports the
misleading message; there is no way to fix an already-installed v0.1.0 other
than upgrading.

Cutting that release exercised the fix's own subject matter: three attempts,
each blocked by a gate that named the real cause (a token scoped to the wrong
repo, then an assertion still looking for the pre-rename log filenames). The
tap was never updated on a failed attempt, so no user could install a formula
whose service story was unverified.

## Lessons

- `LESSON-456` — a `_`-discarded error is a silent downgrade; a fallback
  identifier is not "none"; one classifier per state, not one per surface.

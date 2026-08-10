---
id: TASK-085
title: "Acceptance suite across the nine ACs, plus the dogfood measurement"
status: complete
parent: REQ-564
created: 2026-08-10
updated: 2026-08-10
dependencies: [TASK-082, TASK-083, TASK-084]
---

## Description

Prove the nine acceptance criteria, and state honestly which of them CI can
prove and which needs a real model (architecture "Test strategy").

## Files to Create/Modify

- `crates/tetond/tests/prefix_cache_session.rs` — new: the multi-turn,
  divergence, interleaved-session, duty-interleave, eviction and ledger cases
- `crates/teton-inference/src/prefix_cache.rs` — extend unit tests if the
  suite finds uncovered policy edges
- `docs/manual-verification.md` — record the dogfood measurement procedure and
  its result (this is where the repo already keeps CI-unprovable claims)

## Acceptance Criteria

- [ ] AC-1: a scripted 5-turn session emits `prefix_cache_hit` on turns 2–5 and
      the reported processed-token counts equal the per-turn delta, not the full
      prompt length
- [ ] AC-2: fixed-seed A/B — cache enabled vs disabled produces byte-identical
      output across a multi-turn session
- [ ] AC-3: a turn following a context compaction emits a `divergent` miss and
      still produces correct output
- [ ] AC-4: two sessions alternating turns both produce correct output (thrash
      allowed, wrong output not)
- [ ] AC-5: `evict_prefix_cache` drops the cache, emits `Evicted`, and the next
      turn succeeds cold
- [ ] AC-6: agent turn → summarize duty → agent turn, and the second agent turn
      is still a Hit (BR-5)
- [ ] AC-7: `nonblocking_inference.rs` passes unchanged
- [ ] AC-8: an over-window prompt is refused with the typed error on the hit
      path as well as the miss path, with no process abort
- [ ] AC-9: summed ledger cached/processed counts across AC-1's session match
      the emitted events
- [ ] The scripted test engine implements `complete_cached` via the **same**
      `PrefixCacheState`, not a reimplementation of the rule
- [ ] Dogfood evidence recorded: context create/destroy count over a real
      multi-turn session with `--features tetond/llama`, compared against the
      211-cycle baseline in the requirement

## Technical Notes

The `llama` feature is non-default and CI never compiles it, so AC-1..AC-9 are
proven against a scripted engine. That proves the policy, the plumbing, the
guard and the accounting — it does **not** prove llama.cpp reuses the KV. Say
so in the test module's header rather than letting a green suite imply more
than it shows.

The dogfood run is the only evidence for the actual latency claim. Build the
workspace first: a targeted `-p teton --test …` run does not rebuild `tetond`,
so a change can look verified against a stale daemon.

For AC-2, "cache disabled" is the cold path — drive the same session through
`complete` and through `complete_cached` and compare transcripts.

For AC-8, the hit-path case must be a session whose *earlier* turns populated
the cache and whose final turn exceeds the window, so the guard is reached with
a resident prefix present.

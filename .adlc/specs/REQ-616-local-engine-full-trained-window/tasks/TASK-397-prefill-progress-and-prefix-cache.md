---
id: TASK-397
title: "Size the prefix cache to the new window and emit prefill_progress"
status: complete
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-392, TASK-394]
---

## Description

REQ-564's persistent llama context is sized to the new `n_ctx`, a long first
prefill reports progress so it is visibly working, and the 120 s duty deadline
does not apply to an agent-turn prefill (BR-9, AC-9).

## Files to Create/Modify

- `crates/teton-inference/src/prefix_cache.rs` — size the resident context to the
  fitted window
- `crates/teton-protocol/src/events.rs` — `PrefillProgress` event
- `crates/teton-inference/src/engine.rs` — emit progress during prefill
- `crates/tetond/src/harness/turn_loop.rs` — exempt an agent-turn prefill from
  the duty deadline

## Acceptance Criteria

- [ ] A prefill of more than 32,768 new tokens emits `prefill_progress` with
      `tokens_done`, `tokens_total`, `tokens_per_second`, at most once per second
- [ ] A 100,000-token prefill emits at least once and the turn does not hit a
      deadline (AC-9)
- [ ] A prefill *below* the 32,768 threshold emits nothing — the benign path
- [ ] The rate limit is asserted, not assumed: a fast prefill does not produce a
      burst of events
- [ ] The persistent context is allocated at the fitted window, not the default

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-9 | test-case | `crates/teton-inference/src/prefix_cache.rs::resident_context_uses_the_fitted_window` | no |
| AC-9 | test-case | `crates/tetond/src/harness/turn_loop.rs::long_prefill_reports_progress_and_misses_no_deadline` | yes |

## Technical Notes

- Emission must be driven from a stub/mock engine in tests — the real prefill is
  behind the `llama` feature. Assert on the event stream, not on wall-clock
  timing, so the test is not flaky.
- LESSON-498 applies if the progress callback has to cross the `!Send` FFI
  boundary: a borrowed non-`Send` handle wants a thread, not a struct field.

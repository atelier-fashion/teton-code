---
id: TASK-193
title: "e2e/integration fixtures: privacy-block refit (AC-15a), fallback to a smaller window (AC-15b), carry across a budget drop (AC-11), redact scan bound (AC-6), pressure emissions (AC-10 daemon half)"
status: complete
parent: REQ-586
created: 2026-08-19
updated: 2026-08-19
dependencies: ["TASK-189"]
repo: teton-code
---

## Description

The integration half of TASK-189's wiring: the e2e and carry fixtures that
observe the refit, the cross-turn budget drop, the redact bound and the
pressure emissions end to end. Split out so TASK-189 stays one session
(Phase-3 F-9).

## Files to Create/Modify

- `crates/tetond/tests/e2e/privacy_fixes.rs` — `taint_and_reroute` (L73) extended or a sibling: 128k remote + a large context built at ≤ 4 B/word (so the byte guard does not bind before the reroute — Phase-3 F-19), the privacy-block reroute to the local pin emits `context_pressure { kind: refit_on_reroute }` **before** the local `route_decided`, and the turn completes (AC-15a).
- `crates/tetond/tests/e2e/ac_matrix.rs` — `ac7_degraded_provider_falls_back_and_completes` (L678) extended: primary `max_context = 128000`, fallback `32000`; the fallback attempt is preceded by `refit_on_reroute` and completes with no over-window error (AC-15b); config builder `remote_provider_block` (`e2e/harness.rs:1895`) gains a capabilities variant.
- `crates/tetond/tests/conversation_carry.rs` — the `Carry` fixture (L441-491) gains a per-prompt budget override (`with_budget` today is per fixture); AC-11: a 30,000-word conversation assembled on a 128k pair, next prompt on the default pair → oldest dropped, `context_pressure { blocks_dropped }` emitted, turn completes, and the retained conversation is what the local turn kept (REQ-567 BR-6 atomic commit).
- `crates/tetond/tests/redact_egress.rs` — AC-6: `[privacy] redact = true` + a 128k route: the assembled body is bounded by the scannable bound and the scan forwards (copy `a_context_budget_full_payload_is_scanned_across_windows_and_forwards` L944); this is TASK-192's mutation (i) target.
- `crates/tetond/tests/` (new or in `conversation_carry.rs`) — AC-10 daemon half: a prompt forcing three drops emits exactly one `context_pressure { blocks_dropped: 3 }`; a single oversized newest user block emits `{ block_elided, newest_user_elided: true }` **and** the turn output carries the one-line notice; removing either emission fails.

## Acceptance Criteria

- [x] AC-6, AC-10 (daemon half), AC-11, AC-15a, AC-15b green in the harnesses named above; `cargo test -p tetond --no-fail-fast` green (1,897 passed / 0 failed / 1 ignored).
- [x] Every fixture that claims a word count states its bytes/word so the byte guard is not the thing being tested by accident (Phase-3 F-19).

## What landed where

| Claim | Test | File |
|---|---|---|
| AC-15a privacy-block refit | `a_128k_turn_blocked_by_privacy_is_refitted_before_the_local_pin_serves_it` | `e2e/privacy_fixes.rs` |
| AC-15b fallback to a smaller window | `ac7_degraded_provider_falls_back_and_completes` (extended) | `e2e/ac_matrix.rs` |
| capabilities config builder | `remote_provider_block_with_window` | `e2e/harness.rs` |
| ordered, anchored event lookup | `Client::event_index_from` / `Client::event_names` | `e2e/harness.rs` |
| AC-11 carry across a budget drop | `a_conversation_assembled_on_a_128k_route_survives_a_local_turns_smaller_budget` | `conversation_carry.rs` |
| per-prompt budget override | `Carry::prompt_under` | `conversation_carry.rs` |
| AC-6 redact scan bound | `a_redact_scanned_128k_route_assembles_a_body_the_scan_reads_whole_and_forwards` | `redact_egress.rs` |
| AC-10 three drops → one event | `three_dropped_blocks_are_one_event_naming_all_three` | `context_pressure.rs` (new) |
| AC-10 elided newest user block: event **and** notice; BR-7 marker names the route's window | `an_elided_newest_user_message_is_an_event_and_a_notice_in_the_turns_output` | `context_pressure.rs` (new) |
| AC-9 / TASK-187 handover: 200 blocks compacting through a scripted local engine | `a_two_hundred_block_conversation_on_a_big_route_compacts_through_the_local_binding` | `context_pressure.rs` (new) |

Bytes per word, stated per fixture: AC-15a and AC-15b pastes 4 B/word (30,000
words / 120,000 bytes); AC-11 pastes 4 B/word (12 x 2,500 words = 30,000 words /
120,000 bytes); `context_pressure.rs` filler 4 B/word throughout; AC-6's redact
filler is prose at ~6.4 B/word and says so (the bound it tests is
byte-denominated, so the word figure is incidental and is named as such).

## Mutations executed (each reverted immediately)

- `announce_pressure`'s `events.context_pressure(..)` removed → both AC-10 tests red.
- `announce_pressure`'s `agent_message` notice removed → the elision test red, the drops test still green (the two surfaces are separately pinned).
- `compact_if_pressured`'s `COMPACT_PROMPT_BUDGET_BYTES` replaced with `usize::MAX` → the 200-block test red ("prompt of 205,009 bytes exceeds this engine's window (24,521)").
- `refit_for_reroute` made an early `return` → AC-15a and AC-15b both red.
- AC-6's own mutation is asserted inside the test: the same route derived with `redact_scan = false` assembles 253,952 bytes and is refused `ScanUnavailable`.

## Deviations

- The TASK-187 handover case landed in the new `tests/context_pressure.rs` rather than in `conversation_carry.rs`. It is about the `compact` duty's prompt bound on a big route, not about the carry; `context_pressure.rs` is this task's REQ-586 daemon-level home and the case reads as one of its three rather than as a fourth subject in REQ-567's file.
- `conversation_carry.rs`'s `WIDE_N_CTX` (the fixture engine's window guard) was widened 32,768 -> 65,536. AC-11's conversation is 30,000 words **by requirement** and is assembled whole on the 128k leg; at the old ceiling the peak prompt cleared the guard by ~5%, which is a margin the next system-prompt change would eat. The guard is still the real `over_window` on both arms, and no test in that file expects a refusal.
- The AC-10 elision turn publishes a **second**, later `context_pressure`: the clamp fills the byte budget exactly, so appending the model's reply makes the exit gate drop the (now oldest) clamped block. That is existing REQ-561/567 behaviour, correctly announced as its own clamp; the test pins the elision as the *first* event and as the only one carrying `newest_user_elided`.

## Technical Notes

- **Inherited from TASK-187**: its AC-3 (a 200-block context compacting through a *scripted local engine* in `tests/conversation_carry.rs`) was deferred to you — the unit-level equivalent is green (prompt ≤ 24,521 B, non-empty oldest prefix, block 200 named protected). Add the engine-level case here.

- `e2e/harness.rs` `MockProvider` / `Workspace::write_config` for the real-daemon legs; the `ScriptedSseTransport` shape for anything that needs no daemon.
- Commit as `test(daemon): refit, carry-across-budget, redact bound and pressure emissions end to end [TASK-193]`.

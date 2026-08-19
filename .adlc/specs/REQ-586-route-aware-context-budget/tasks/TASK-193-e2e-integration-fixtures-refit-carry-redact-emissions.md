---
id: TASK-193
title: "e2e/integration fixtures: privacy-block refit (AC-15a), fallback to a smaller window (AC-15b), carry across a budget drop (AC-11), redact scan bound (AC-6), pressure emissions (AC-10 daemon half)"
status: draft
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

- [ ] AC-6, AC-10 (daemon half), AC-11, AC-15a, AC-15b green in the harnesses named above; `cargo test -p tetond --no-fail-fast` green.
- [ ] Every fixture that claims a word count states its bytes/word so the byte guard is not the thing being tested by accident (Phase-3 F-19).

## Technical Notes

- `e2e/harness.rs` `MockProvider` / `Workspace::write_config` for the real-daemon legs; the `ScriptedSseTransport` shape for anything that needs no daemon.
- Commit as `test(daemon): refit, carry-across-budget, redact bound and pressure emissions end to end [TASK-193]`.

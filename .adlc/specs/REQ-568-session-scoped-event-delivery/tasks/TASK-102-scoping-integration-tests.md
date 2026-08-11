---
id: TASK-102
title: "Integration: multi-client scoping e2e, fence variants, full-suite green"
status: draft
parent: REQ-568
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-099", "TASK-100", "TASK-101"]
---

## Description

The cross-cutting acceptance evidence: socket-level multi-client scoping tests
(spec AC-1/2/3/4/6), fence no-hang under filtering, seq-gap tolerance, monitor
observability, and a full-workspace suite run (AC-7). This task turns the
spec's acceptance criteria into named tests and checks them off in the
requirement.

## Files to Create/Modify

- `crates/tetond/tests/multi_client.rs` — new tests using the existing `TestClient` raw-NDJSON pattern: (a) **AC-1**: two clients, two sessions; assert at the socket that B receives none of A's session envelopes across a full prompt turn while both receive a daemon-scoped envelope; (b) **AC-2**: a handshaked never-attached client receives only daemon-scoped envelopes over a drain window; (c) **AC-3**: a `monitor: true` client receives both sessions' envelopes; (d) **AC-6 + seq gaps**: during filtered delivery, a response gated on the fence completes within the test window (no hang) and B's observed seq values are monotonic but non-contiguous — assert monotonicity, assert at least one gap, never assert contiguity.
- `crates/tetond/tests/event_response_ordering.rs` — extend the racing ordering test with a second, filtered subscriber (attached to a different session) alive during the 150-turn run: the prompting client's events still precede its responses AND the filtered client's connection never stalls the prompting client (ADR-A regression net).
- `crates/tetond/tests/e2e/harness.rs` — `Client::handshake` gains an opt-in monitor knob (builder-style, default off) so e2e tests can declare monitor; used by (c).
- `.adlc/specs/REQ-568-session-scoped-event-delivery/requirement.md` — tick the AC checkboxes as each lands, with the test name beside it.

## Acceptance Criteria

- [ ] AC-1, AC-2, AC-3, AC-4, AC-6 each pinned by a named test listed above (AC-4's test landed in TASK-099 — reference it, don't duplicate).
- [ ] Seq-gap tolerance asserted (monotonic, non-contiguous) — nothing anywhere assumes contiguity.
- [ ] Full workspace build + test green: `cargo build --workspace && cargo test --workspace` (AC-7; build first so the e2e suite tests the fresh daemon — the stale-daemon trap in targeted runs).
- [ ] Spec AC checkboxes updated with test names.

## Technical Notes

- Run the workspace suite with `--no-fail-fast` when a failure appears, so the reported count is a total, not a floor.
- `Client` in harness.rs auto-answers permission prompts; the scoping tests must use sessions with the scripted local engine (`TETON_LOCAL_SCRIPT` pattern used across e2e) so no real model is needed.
- Timing discipline: "B receives none of A's envelopes" is a negative assertion — bound it with the positive control in the same test (A's own client DID receive them, and B received the daemon-scoped marker), so the test can't pass by the daemon being slow.

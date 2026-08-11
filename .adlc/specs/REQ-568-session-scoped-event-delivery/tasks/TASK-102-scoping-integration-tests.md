---
id: TASK-102
title: "Integration: multi-client scoping e2e, fence variants, full-suite green"
status: complete
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
- `crates/tetond/tests/e2e/harness.rs` — **not changed; see "The harness monitor knob" below.**
- `.adlc/specs/REQ-568-session-scoped-event-delivery/requirement.md` — tick the AC checkboxes as each lands, with the test name beside it.

## Acceptance Criteria

- [x] AC-1, AC-2, AC-3, AC-4, AC-6 each pinned by a named test listed above (AC-4's test landed in TASK-099 — reference it, don't duplicate).
- [x] Seq-gap tolerance asserted (monotonic, non-contiguous) — nothing anywhere assumes contiguity.
- [x] Full workspace build + test green: `cargo build --workspace && cargo test --workspace` (AC-7; build first so the e2e suite tests the fresh daemon — the stale-daemon trap in targeted runs).
- [x] Spec AC checkboxes updated with test names.

## What landed

New, in `crates/tetond/tests/multi_client.rs`:

| Test | Pins |
|------|------|
| `two_clients_prompting_their_own_sessions_see_only_their_own_envelopes` | AC-1 — two clients, two sessions, a full prompt turn on each; neither sees the other's envelopes, both reach the daemon-scoped marker |
| `a_client_that_never_attached_receives_only_daemon_scoped_envelopes` | AC-2 |
| `a_filtered_client_sees_gapped_seqs_and_its_fenced_response_still_completes` | AC-6 + ADR-A's seq-gap consequence |

New, in `crates/tetond/tests/event_response_ordering.rs`:
`a_turns_ordering_holds_while_another_client_holds_a_different_session` — the
racing 150-turn run with a filtered peer attached to a different session alive
throughout. Iteration count: the existing `TURNS`, unchanged — the peer is
passive (one connection, one session, two requests), so it adds no turns and no
measurable runtime.

Referenced rather than duplicated: `mutating_methods_are_refused_until_the_connection_attaches`
(AC-4, TASK-099), `a_monitor_declared_at_handshake_receives_another_clients_events`
(AC-3, TASK-098), `frame_cap.rs` (AC-5, TASK-100), `the_pump_renders_its_own_session_and_daemon_scope_only`
(AC-8, TASK-101).

Full workspace, built first so the e2e suite exercises a fresh daemon rather
than a stale one: `cargo build --workspace && cargo test --workspace
--no-fail-fast` → **1989 passed / 0 failed / 1 ignored across 43 test targets**
(the ignored case predates this REQ). `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets -- -D warnings` are clean.

Each new negative assertion was checked against a mutant, not just run green:
with `should_forward`'s scoped arm forced to `true` all three new
`multi_client` tests and the ordering variant fail; with a skipped envelope no
longer advancing the forwarded watermark (BR-7 inverted), the AC-6 test times
out on its fenced `session/list`. The negatives are therefore load-bearing
rather than vacuous.

### The harness monitor knob

Not added. AC-3's evidence belongs at the socket layer and already lives there:
`multi_client.rs` drives raw NDJSON, so it observes the delivery decision at the
seam BR-3 names, while `e2e/harness.rs`'s `Client` is a convenience wrapper that
pumps and auto-answers — a monitor assertion written through it would be testing
the wrapper's pump, one layer above the filter. The only consumer the knob could
have had is the test that already exists without it, so adding a builder-style
`monitor` opt-in to `Client::handshake` here would have shipped an unused public
API into the e2e harness. Skipped deliberately; if a future e2e test genuinely
needs to declare monitor, the knob is three lines and lands with its consumer.

## Technical Notes

- Run the workspace suite with `--no-fail-fast` when a failure appears, so the reported count is a total, not a floor.
- `Client` in harness.rs auto-answers permission prompts; the scoping tests must use sessions with the scripted local engine (`TETON_LOCAL_SCRIPT` pattern used across e2e) so no real model is needed.
- Timing discipline: "B receives none of A's envelopes" is a negative assertion — bound it with the positive control in the same test (A's own client DID receive them, and B received the daemon-scoped marker), so the test can't pass by the daemon being slow.

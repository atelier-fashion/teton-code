---
id: TASK-122
title: "Fail-closed malformed-provenance guard with a client-visible event"
status: draft
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-4 (ADR-D): reject a malformed provenance source at the egress
inspection point, before boundary matching, whether or not a boundary is
configured — and report it on the protocol rather than only to daemon logs.

Protocol variant and CLI arm land together: the `Event` match in
`crates/teton/src/session_ui.rs:273-494` is exhaustive (verified — no wildcard),
so splitting them would leave the workspace un-buildable between tasks.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `ProvenanceRejected` struct; `Event::ProvenanceRejected` variant; arm in `Event::name()`.
- `crates/tetond/src/egress/inspector.rs` — fail-closed well-formedness check ahead of boundary matching.
- `crates/tetond/src/egress/mod.rs` — publish the event through the existing sink.
- `crates/teton/src/session_ui.rs` — render the new variant.
- `crates/tetond/tests/provenance_rejection.rs` — new. AC-5 and AC-14.

## Acceptance Criteria

- [ ] AC-5: a unit test asserts the rejection fires for an absolute source and for a `..`-bearing source, **with no boundary configured**.
- [ ] AC-14: an integration test asserts `provenance_rejected` is delivered to a subscribed client, not merely logged (LESSON-505).
- [ ] The guard runs BEFORE boundary matching and fails closed — a malformed source is never matched-and-passed.
- [ ] The workspace builds at this commit: the `session_ui` arm is present in the same change as the enum variant.
- [ ] `PROTOCOL_VERSION` is unchanged, with a note recording why the addition is wire-compatible.
- [ ] The test carries a comment stating the guard is redundant by construction and why it is tested anyway (LESSON-508), so it is not deleted as noise.
- [ ] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

Follow `PrivacyBlock` (`teton-protocol/src/events.rs:361-384`) for the struct
shape and `emit_provider_degraded` (`crates/tetond/src/router.rs:787-793`) for
publication. Event capture in tests: subscribe to the bus before driving, then
`tokio::time::timeout` on `recv` — the idiom at `provenance_egress.rs:232-239`.

ADR-A should make this guard unreachable from first-party tools. It stays
because `ProvenanceId::claimed()` accepts third-party MCP assertions, and
because a redundant guard with no test is one refactor from being deleted.

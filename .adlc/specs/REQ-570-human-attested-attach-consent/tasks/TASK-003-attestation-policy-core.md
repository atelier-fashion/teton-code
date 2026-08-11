---
id: TASK-003
title: "Attestation policy core: binding, single-use, expiry, refusal taxonomy (feature-free)"
status: pending
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-001]
---

## Description

The pure half of the attestation subsystem, per architecture.md §2 (ADR-B) and
the project's established "policy is pure, mechanism is gated" pattern
(REQ-564, LESSON-499). Every BR-6/BR-7 rule is decided here over plain data, so
it is table-testable with no FFI, no daemon and no socket — which matters
because the mechanism sits behind a non-default cargo feature CI never compiles,
and otherwise the subtlest code in the tree would ship with the least coverage.

## Files to Create/Modify

- `crates/tetond/src/attest/mod.rs` — module root, `AttestationMethod`,
  `MechanismAvailability`, the `PresenceVerifier` trait (the AC-7 injection seam).
- `crates/tetond/src/attest/policy.rs` — `PresenceAttestation`,
  `AttestationRegistry`, `AttestationRefusal`. **No `#[cfg(feature)]` anywhere.**
- `crates/tetond/src/lib.rs` — declare the module.

## Acceptance Criteria

- [ ] AC-5: an attestation is single-use and expires. Replaying it, or using it
      for a different `request_id` or a different `ConnectionId`, is refused.
      Table-tested across all four cells (right/wrong subject x right/wrong request).
- [ ] BR-6: `(subject, request)` is the whole key — following LESSON-495 as
      REQ-569's grants already do, so an attestation cannot answer a question it
      was not minted for.
- [ ] Single-use is a **consuming take** from the registry, not a boolean flag a
      caller must remember to set (mirrors REQ-569's `route_of`-read /
      `resolve`-consume split).
- [ ] AC-6: failure, cancellation and timeout are distinct values; each leaves
      the registry **empty**, asserted by inspecting the registry rather than
      inferring from the error.
- [ ] `AttestationMethod::None` never mints — enforced by the registry refusing,
      not by callers checking.
- [ ] Expiry is 60s, single-use, no burst coverage (closes OQ-3 — see §2).
- [ ] `cargo test -p tetond --no-fail-fast` green.

## Technical Notes

- `out_of_band_code` is deliberately **absent** from `AttestationMethod`: OQ-1
  dropped it, and an unreachable variant in a security enum invites a future
  reader to wire it up.
- Mirror `grants.rs`'s two-halves shape: pure functions + a locked registry that
  answers by calling them, so there is exactly one definition of each rule.

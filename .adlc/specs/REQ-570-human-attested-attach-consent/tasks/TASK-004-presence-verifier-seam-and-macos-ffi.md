---
id: TASK-004
title: "PresenceVerifier seam, macOS LAContext mechanism, and the fail-closed posture"
status: complete
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-003]
---

## Description

The gated mechanism half, plus the injected seam that makes the
no-mechanism posture assertable on any platform. Consumes TASK-003's policy —
the test double uses that **same** policy, never a reimplementation (LESSON-499:
a double with its own copy of the rule tests only that two implementations share
each other's bugs).

Empirical parameters come from architecture.md §0, not from documentation:
`deviceOwnerAuthentication` (policy 2), and the LAError code mapping the spike
observed.

## Files to Create/Modify

- `crates/tetond/src/attest/mechanism.rs` — `#[cfg(feature = "presence")]`.
  LAContext FFI and **nothing else**; it answers "did a human authenticate" and
  maps LAError into the policy module's refusal enum. Holds no policy.
- `crates/tetond/src/attest/mod.rs` — `UnavailableVerifier` (the always-refuses
  implementation) and a test double.
- `crates/tetond/Cargo.toml` — the non-default `presence` feature.

## Acceptance Criteria

- [x] AC-7: on a platform with no usable mechanism, cross-session attach is
      refused with the BR-8 posture code — **never** self-approved. Asserted via
      the injected "no mechanism available" seam so it runs on any platform.
- [x] AC-7b: the Linux-without-a-polkit-agent case reaches that refusal and names
      **that** cause, not a generic failure — and is reachable in CI on a
      headless Linux runner.
- [x] `MechanismAvailability::Unavailable { reason }` is a first-class value, not
      an error path; the reason enum carries `NoPolkitAgent` specifically.
- [x] BR-11: a degraded platform never falls back to the REQ-569 self-approval
      residual. Asserted as a refusal, not as an absence.
- [x] LAError -1/-2/-4/-9 map to distinct refusals (BR-7), per §0's finding.
- [x] Default/CI builds compile none of the FFI and keep the honest-refusal
      behaviour.

## Technical Notes

- The refusal keys on **agent availability**, not on "is polkit installed" — §0
  found the authority can be on the bus and still answer "no agent", so the
  latter is a false positive.
- Do not add a textual-agent fallback: §0 found it needs `/dev/tty`, which
  neither headless Linux nor the VS Code extension has.

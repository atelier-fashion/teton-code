---
id: TASK-141
title: "AC-1 disk-inspection e2e for RegisterProvider/SetPrivacyBoundary + the off-dispatch routing pin"
status: complete
parent: REQ-576
created: 2026-08-14
updated: 2026-08-14
dependencies: [TASK-140]
repo: teton-code
---

## Description

Add the coverage the shared daemon_wide harness does not provide: AC-1's
"inspect, don't infer" proof against a **real config file** for the two
egress/privacy-critical `ConfigUpdate` variants, plus the config/set-specific
"left the reader-loop dispatch" pin. See `architecture.md` ADR-4 and LESSON-519.

## Files to Create/Modify

- `crates/tetond/tests/multi_client.rs` (or a config-set integration file that
  spawns a real daemon with a config path) — AC-1: with
  `TETON_TEST_SEAMS=1` + `TETON_PRESENCE_ACCEPT=fail` (the REQ-575 seam →
  `AlwaysFailsVerifier`), a `config/set` `RegisterProvider` **and** a
  `SetPrivacyBoundary`, from an ancestry-passing connection, are each refused with
  `ATTESTATION_FAILED`; assert the on-disk `config.toml` is **byte-identical**
  before/after and the live config (via `config/get`) is unchanged — read back,
  not inferred from the error code.
- `crates/tetond/src/server.rs` (test module) — a routing pin: `dispatch(...,
  ConfigSetParams::METHOD, ...)` answers `METHOD_NOT_FOUND` (config/set left the
  synchronous dispatch and runs on `blocks_on_a_human`), complementing the
  integration suites that still reach it over the socket. Mirror REQ-575's
  `the_commit_left_the_reader_loop_dispatch_while_the_reads_stayed`.

## Acceptance Criteria

- [ ] AC-1 (RegisterProvider): refused with `ATTESTATION_FAILED`; config.toml
      byte-identical on disk; live config unchanged — all by inspection.
- [ ] AC-1 (SetPrivacyBoundary): same, for the privacy-boundary variant — the
      one the spec calls out as directly mutating the privacy promise.
- [ ] Non-vacuity: pair with the accepting/served path (a `config/set` under
      `TETON_PRESENCE_ACCEPT=1` or the default degrade) that **does** change the
      file, so a regression that let a refused config/set write flips the refusal
      assertions.
- [ ] Routing pin: `dispatch` answers `METHOD_NOT_FOUND` for config/set; the
      integration suites still reach it over the socket.
- [ ] Reader-loop liveness is inherited (config/set uses the identical
      `blocks_on_a_human` machinery REQ-575's
      `a_parked_web_setup_commit_does_not_stall_the_connection` pins) — reference
      that in the routing-pin doc comment rather than duplicating the ParkingVerifier.
- [ ] `cargo test -p tetond` (targeted integration + lib) green after a workspace build.

## Technical Notes

- The `fail` seam already exists (`attest/mod.rs`, REQ-575). No mechanism change.
- Reuse the config-path spawned-daemon harness the existing config/set
  integration tests use (they already write a config and call `config/set` over
  the socket) — add the seam env and the inspection, don't build a new harness.
- AC-3 (mutation) is NOT here — it is covered by TASK-140's `commitments`-list
  addition to `only_a_daemon_wide_commitment_demands_presence`.

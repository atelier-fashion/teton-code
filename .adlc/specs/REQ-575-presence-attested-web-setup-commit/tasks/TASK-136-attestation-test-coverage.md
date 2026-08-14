---
id: TASK-136
title: "Pin the new attestation seam: refusal, mutation, degradation, ordering, off-dispatch"
status: complete
parent: REQ-575
created: 2026-08-14
updated: 2026-08-14
dependencies: [TASK-135]
repo: teton-code
---

> **Implementation note (reconciled during Phase 4, updated after the verify
> pass).** `web_setup_flow.rs` is a *spawned-binary* harness (env-driven
> `DaemonOptions`), so a live-refusing verifier cannot be injected there — the
> AlwaysFailsVerifier/degradation gating tests live in-process in `server::tests`.
> **AC-2 (accepting-verifier full flow) moved to TASK-137**, where the
> spawned-binary `TETON_PRESENCE_ACCEPT` seam is the only way to reach the granted
> path over the real socket; it is the same test as AC-6.
>
> The **reader-loop liveness property** is pinned two ways: structurally by
> `the_commit_left_the_reader_loop_dispatch_while_the_reads_stayed` (the commit is
> no longer served inline by `dispatch`), and — added in the verify pass, after
> the 6-agent review judged structural coverage insufficient — behaviourally by
> `a_parked_web_setup_commit_does_not_stall_the_connection` (multi_client.rs): a
> `ParkingVerifier` blocks inside `verify` on a multi-thread runtime (the
> production `block_in_place` branch), and a concurrent `session/list` on the same
> connection is served while the commit is parked. AC-1's disk-inspection clause
> is likewise satisfied over the socket by a new `TETON_PRESENCE_ACCEPT=fail`
> seam + `a_presence_refused_commit_writes_nothing_and_swaps_nothing`
> (web_setup_flow.rs), which reads the config bytes and live state back rather
> than inferring from the error code.

## Description

Add the coverage that proves the new presence gate is load-bearing at its own
seam (LESSON-502/508), that the granted path still works end to end, and that
the reader loop stays free while the commit parks on a human. See
`architecture.md` ADR-3.

## Files to Create/Modify

- `crates/tetond/src/server.rs` (test module) — (1) AC-1: with a daemon carrying
  `AlwaysFailsVerifier`, a `handle_web_setup_commit(...).await` from a **properly
  attached** connection returns the attestation error code, and the runtime is
  never reached — assert the on-disk config path is untouched and the in-memory
  config is unchanged (inspect state, do not infer from the error). (2) AC-5: the
  mutation test — deleting the `refuse_unattested_commitment` line from
  `handle_web_setup_commit` makes this test red, independently of the
  model/confirm+model/set seams. (3) AC-3: with the shipped no-mechanism verifier
  (`UnavailableVerifier`), the commit is NOT refused by attestation (it proceeds
  to the runtime) and the stated degradation notice path is exercised.
- `crates/tetond/src/server.rs` (test module) — (AC-4, ordering / BR-2) with
  `AlwaysFailsVerifier` installed (a verifier that WOULD refuse if reached), a
  commit from an **unattached** connection returns `NOT_ATTACHED` — **not** the
  attestation error — proving the session gate fires before the verifier is
  consulted; and a commit with an **unmintable** session id returns
  `INVALID_PARAMS` before the verifier is consulted. `AlwaysFailsVerifier` is
  the tripwire: if the attestation check ran first, these callers would get the
  attestation refusal instead, and the assertions fail.
- `crates/tetond/tests/web_setup_flow.rs` — (4) AC-2: with
  `AcceptingVerifier::default()` installed via `with_presence_verifier`, the full
  plan → preview → commit → same-session live lookup passes unchanged. (5) the
  reader-loop-free assertion mirroring `model_consent.rs` (~2328): a
  `web/setup_commit` that must attest does not stall a second RPC issued on the
  same connection.

## Acceptance Criteria

- [ ] AC-1 present-but-refusing verifier: commit refused with the attestation
      code; config on disk byte-identical; in-memory config not swapped — all
      asserted by inspection.
- [ ] AC-4 ordering (BR-2): with `AlwaysFailsVerifier` as a tripwire, an
      unattached caller gets `NOT_ATTACHED` and an unmintable session id gets
      `INVALID_PARAMS` — both refused before the verifier is consulted, so no
      prompt can appear for a caller that may not act.
- [ ] AC-5 mutation: removing the attestation line turns at least one of these
      tests red, and does so independently of the model-method seams.
- [ ] AC-3 degradation: no-mechanism verifier → commit lands, stated notice, zero
      new prompts.
- [ ] AC-2 granted path: accepting verifier → full flow + live pickup green,
      unchanged from the pre-REQ-575 behavior.
- [ ] Reader-loop-free: a parked commit does not block a concurrent RPC on the
      same connection.
- [ ] `cargo test -p tetond` (and the `web_setup_flow` integration target) green.

## Technical Notes

- Mirror the attested-refusal pattern already in `crates/tetond/tests/model_consent.rs`
  and its `with_presence_verifier(Box::new(AcceptingVerifier::default()))` usage.
- `AlwaysFailsVerifier` (present, always refuses) is the right double for AC-1/AC-5
  — NOT `UnavailableVerifier`, which degrades on a commitment path (that is AC-3's
  double). The distinction is REQ-570's and must be preserved.
- Build the workspace before the targeted `-p tetond --test web_setup_flow` run so
  the test drives a current daemon, not a stale one.
- Keep each test's failure message stating the security property it pins, per the
  repo's existing test-comment convention.

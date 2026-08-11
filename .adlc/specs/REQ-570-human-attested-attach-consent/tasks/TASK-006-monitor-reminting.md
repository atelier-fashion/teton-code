---
id: TASK-006
title: "monitor becomes mintable again, and only under attestation"
status: pending
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-005]
---

## Description

Gap 3: REQ-569 removed the monitor consent path after finding it self-serviceable
(one attacker holding two connections), leaving `monitor` grant-gated and
therefore a dead REQ-568 capability. It becomes mintable again only now that a
human-attested surface exists, because a monitor is a whole-daemon read and
nothing weaker should mint it.

## Files to Create/Modify

- `crates/tetond/src/consent.rs` — a monitor arm, under two conditions.
- `crates/tetond/src/server.rs` — the monitor minting path and its refusal.

## Acceptance Criteria

- [ ] AC-2: the REQ-569 two-connection attack (conn A creates a throwaway
      session, conn B requests monitor, A approves) is refused, as a **named
      regression test**.
- [ ] AC-2b: `monitor` is mintable — a connection presenting a valid attestation,
      answering a monitor-scope request it did **not** raise, receives the grant
      and can then monitor. Asserted **positively**: a capability only ever
      observed being refused is indistinguishable from the dead code Gap 3
      describes.
- [ ] BR-5: the approver is never the requester **under any arm** — checked
      structurally, not merely avoided by construction (LESSON-502).
- [ ] `NOT_GRANTED_MESSAGE` is updated: it currently says the daemon "has no way
      to mint one over the socket", which stops being true.

## Technical Notes

- The attestation is what breaks the two-connection attack — the second
  connection cannot produce one without a human at the machine. The
  not-the-requester check remains as defense in depth **with its own test**,
  because REQ-569's attack used two distinct `ConnectionId`s and so did not even
  read as self-approval.

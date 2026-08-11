---
id: TASK-007
title: "grant_minted carries the attestation method"
status: pending
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-005]
---

## Description

BR-9/AC-9: every grant mint stays observable via the daemon-scoped `grant_minted`
event REQ-569 added, now carrying the attestation method — so an operator can
tell an attested grant from a creator-path attach.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `SessionGrantMinted` gains the method.
- `crates/tetond/src/server.rs` — populate it at the announcement site.

## Acceptance Criteria

- [ ] AC-9: `grant_minted` carries the attestation method and is delivered to
      **every handshaked connection**.
- [ ] `AttestationMethod::None` is reported honestly for the creator path rather
      than omitted — a missing field and "no attestation" must not be the same
      wire shape.
- [ ] REQ-569's announcement budget (`GrantAnnouncementBudget`, R3) still bounds
      the notices; this task must not regress it.

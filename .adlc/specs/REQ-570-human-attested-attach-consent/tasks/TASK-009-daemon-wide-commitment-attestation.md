---
id: TASK-009
title: "BR-10(b): a daemon-wide commitment additionally requires attestation"
status: complete
parent: REQ-570
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-005]
---

## Description

Layer (b) of BR-10, on top of TASK-002's layer (a). A daemon-wide *commitment* —
a model change, a multi-GB download — additionally requires a presence
attestation, because its blast radius is the whole machine rather than one
session.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — `handle_model_confirm` and `handle_model_set`.

## Acceptance Criteria

- [x] AC-10 layer (b): a daemon-wide commitment refuses when no valid attestation
      is presented.
- [x] Layer (a) of AC-10 still passes **independently** of the attestation
      mechanism — verified by running the TASK-002 tests with the `presence`
      feature off.
- [x] The read-only siblings (`config/get`, `cost/query`, `web/refresh`) do
      **not** require attestation — they are layer (a) only, per §1's table.
- [x] AC-8: first-run model consent for an ordinary interactive user does not
      gain a second prompt beyond the one it already shows.

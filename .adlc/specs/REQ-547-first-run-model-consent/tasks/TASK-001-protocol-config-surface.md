---
id: TASK-001
title: "Protocol + config surface for model consent"
status: draft
parent: REQ-547
created: 2026-07-21
updated: 2026-07-21
dependencies: []
---

## Description

Wire types and config keys the rest of the REQ builds on: the proposal/decision
events, the `model/*` methods, and the user-authored config inputs.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — add `model_selection_proposed` (ProbeReport + proposed entry + alternatives + required disk) and `model_selection_decided` (chosen name or declined, source)
- `crates/teton-protocol/src/methods.rs` — add `model/confirm` (accept | choose{name} | decline), `model/list`, `model/set`, `model/status`
- `crates/teton-core/src/config.rs` — add `[local_model]` inputs: `pinned` (model name), `auto_accept` (bool, default false), `base_url` override (BR-16); validation for each
- `crates/teton-core/src/entities.rs` — `ModelSelection` (model_name, source enum, declined_local, decided_at) and `ProbeReportView` if a shared shape is needed

## Acceptance Criteria

- [ ] All new payloads round-trip serde JSON and tolerate unknown fields (forward compat), matching the existing protocol test style
- [ ] `model/confirm` params model the three outcomes as a closed enum — an unknown variant is a typed error, never a silent default
- [ ] Config round-trips TOML; `auto_accept` defaults to **false** (BR-5 is opt-in); an invalid `pinned` name or malformed `base_url` fails validation with an actionable message
- [ ] No transport/async code in teton-protocol; teton-core stays I/O-free (`cargo tree` shows no new I/O deps)
- [ ] Tests: payload round-trips, closed-enum rejection, config default + validation cases

## Technical Notes

Follow the `permission_request` / `permission/respond` pair as the naming and
shape precedent (D-3). Event names must match the spec's Events table exactly.
Keep `ModelSelection` free of absolute paths — BR-11 forbids them in protocol
payloads (the install path is CLI-local only).

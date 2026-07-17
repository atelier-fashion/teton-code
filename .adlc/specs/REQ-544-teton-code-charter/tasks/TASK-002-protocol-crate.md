---
id: TASK-002
title: "teton-protocol: JSON-RPC types, events, handshake"
status: draft
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-001]
---

## Description

Define the client↔daemon protocol per ADR-002: JSON-RPC 2.0 framing, request/
response/notification types, the event-subscription envelope, and the version
handshake. ACP-informed vocabulary: session, prompt turn, permission-request,
diff. Serde types only — no transport code (that's TASK-004).

## Files to Create/Modify

- `crates/teton-protocol/src/lib.rs` — module layout, protocol version const
- `crates/teton-protocol/src/jsonrpc.rs` — JSON-RPC 2.0 framing types, id correlation, error codes
- `crates/teton-protocol/src/methods.rs` — typed requests: attach/handshake, session create/list/attach, prompt turn, permission response, config ops
- `crates/teton-protocol/src/events.rs` — event envelope + payloads: `session_update`, `route_decided`, `privacy_block`, `cost_recorded`, `provider_degraded`, `model_lifecycle` (download/benchmark progress), `permission_request`
- `crates/teton-protocol/src/handshake.rs` — version negotiation (client min/max, daemon picks)

## Acceptance Criteria

- [ ] All method/event payloads round-trip serde JSON (unit tests per type)
- [ ] Unknown-field tolerance: deserializing payloads with extra fields succeeds (forward compat)
- [ ] Handshake rejects incompatible version ranges with a typed error
- [ ] Event names match the spec's Events table (REQ-544 System Model)
- [ ] No transport, socket, or async code in this crate

## Technical Notes

Borrow ACP naming where concepts overlap (sessionId, promptTurn,
permissionRequest, diff shapes) so the post-MVP ACP shim is mostly renames —
document each borrowed name with a comment referencing ACP. Error codes:
reserve JSON-RPC standard range; app errors start at -32000.

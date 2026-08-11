---
id: TASK-097
title: "Protocol: HandshakeParams.monitor field and NOT_ATTACHED error code"
status: draft
parent: REQ-568
created: 2026-08-11
updated: 2026-08-11
dependencies: []
---

## Description

Add the two protocol surfaces REQ-568 needs: an additive `monitor: bool` on
`HandshakeParams` (backward-compatible, defaults false) and the `NOT_ATTACHED`
application error code (`-32009`), distinct from `UNKNOWN_SESSION` (`-32001`)
per ADR-B.

## Files to Create/Modify

- `crates/teton-protocol/src/handshake.rs` — add `monitor: bool` to `HandshakeParams` with `#[serde(default)]` (and `skip_serializing_if` matching the file's optional-field idiom, cf. `SessionCreateParams.cwd` in methods.rs); doc-comment: "receive every session's events; explicit opt-in, logged by the daemon (REQ-568 BR-5)". Extend the existing round-trip tests: (a) params WITH `monitor: true` round-trip; (b) legacy JSON WITHOUT the field deserializes to `monitor == false` (backward compat — old client never fails handshake).
- `crates/teton-protocol/src/jsonrpc.rs` — add `NOT_ATTACHED = -32009` to the `application_error_codes!` macro block with a comment ("session exists (or not) but the connection is not attached to it; REQ-568 BR-4"). The existing uniqueness-guard test picks it up automatically; verify it compiles into `ALL`.

## Acceptance Criteria

- [ ] `HandshakeParams` JSON lacking `monitor` deserializes with `monitor == false` (test asserts on a hand-written legacy JSON literal, not on re-serialized output — LESSON-490: fixtures through the real decode path).
- [ ] `monitor: true` survives a serialize→deserialize round trip.
- [ ] `error_code::NOT_ATTACHED == -32009`, present in the macro's `ALL` array, uniqueness guard green.
- [ ] `cargo test -p teton-protocol` passes.

## Technical Notes

- Do NOT touch `negotiate_from` or version admission — monitor is post-version, orthogonal to negotiation (ADR-C).
- No `HandshakeResult` change: the daemon does not echo monitor back; observability is the daemon log (BR-5), wired in TASK-098.
- No capability negotiation of any kind — filtering is unconditional daemon-side.

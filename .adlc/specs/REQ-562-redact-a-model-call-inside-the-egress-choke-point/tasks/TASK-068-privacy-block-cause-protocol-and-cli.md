---
id: TASK-068
title: "privacy_block gains an additive BlockCause; CLI renders the three causes"
status: draft
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: []
repo: teton-code
---

## Description

Extend `teton-protocol`'s `PrivacyBlock` with `cause: BlockCause`
(`#[serde(default)]`, default `Boundary`) per ADR-7: variants `Boundary`,
`Redaction { kind, span }`, `ScanUnavailable`. Teach the CLI to render the
three causes distinctly. Existing emit sites compile unchanged apart from the
defaulted field.

## Files to Create/Modify

- `crates/teton-protocol/src/events.rs` — `BlockCause` enum, `cause` field on `PrivacyBlock`, serde tests
- `crates/tetond/src/egress/mod.rs` — existing boundary-block emission constructs `cause: BlockCause::Boundary` explicitly
- `crates/teton/src/main.rs` (or wherever privacy_block renders) — render boundary vs redaction vs scan-unavailable distinctly; the redaction line shows kind + byte span + the "scan could not run" wording for unavailable
- `crates/tetond/src/runtime.rs` — only if `TaintingPrivacySink` or turn-failure sentences pattern-match on `PrivacyBlock` fields and need the new field threaded

## Acceptance Criteria

- [ ] A serialized v1-era `privacy_block` JSON payload (no `cause` key) deserializes to `cause: Boundary` — asserted by a fixture test with a literal JSON string (backward compatibility is the claim; test it, don't comment it — LESSON-486).
- [ ] The three causes serialize distinctly and round-trip.
- [ ] `Redaction` cause carries `kind` and `span` ONLY — no field can carry matched text (BR-6 structural at the protocol layer too).
- [ ] CLI rendering: three distinguishable lines; the `ScanUnavailable` wording says the scan could not run, not that something was found (BR-3's legibility requirement); no rendering path interpolates payload content.
- [ ] Grep the protocol crate's docs/comments for claims about the change and keep tense honest (LESSON-486).
- [ ] `cargo test -p teton-protocol -p teton -p tetond` green.

## Technical Notes

- Additive-with-default is the compatibility posture (ADR-7): removals/renames
  are what the handshake gate exists for; a defaulted addition must not trip it.
  Confirm against how e523d3d gated ConfigSnapshot — if the protocol has an
  event-version surface, note explicitly why this addition does not bump it.
- `path` keeps its current meaning for boundary blocks; for redaction blocks the
  emitter (TASK-070) will fill a non-secret locus string — this task only makes
  the shape able to carry it.
- Session taint marking (`TaintingPrivacySink::privacy_block`) must keep firing
  for ALL causes — a redaction block taints the session exactly like a boundary
  block (the sink is cause-agnostic; assert nothing filters on cause).

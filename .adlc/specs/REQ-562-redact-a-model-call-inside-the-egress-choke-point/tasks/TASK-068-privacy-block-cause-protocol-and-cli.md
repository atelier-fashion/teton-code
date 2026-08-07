---
id: TASK-068
title: "privacy_block gains an additive BlockCause; CLI renders the three causes"
status: complete
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

- [x] A serialized v1-era `privacy_block` JSON payload (no `cause` key) deserializes to `cause: Boundary` — asserted by a fixture test with a literal JSON string (backward compatibility is the claim; test it, don't comment it — LESSON-486). `events::tests::a_privacy_block_with_no_cause_key_reads_as_a_boundary_block`, with a non-vacuity leg proving the default is not swallowing a `cause` that *is* present; the opposite direction (a build predating the field reading a frame that carries one) is `a_client_predating_the_cause_field_still_reads_a_frame_that_carries_one`.
- [x] The three causes serialize distinctly and round-trip — `events::tests::the_three_block_causes_round_trip_and_serialize_distinctly`.
- [x] `Redaction` cause carries `kind` and `span` ONLY — no field can carry matched text (BR-6 structural at the protocol layer too). `events::tests::a_redaction_cause_carries_only_a_kind_and_a_span` asserts the wire key set exhaustively, so a later text-carrying field turns it red.
- [x] CLI rendering: three distinguishable lines; the `ScanUnavailable` wording says the scan could not run, not that something was found (BR-3's legibility requirement); no rendering path interpolates payload content. `session_ui::tests::the_three_block_causes_render_as_three_distinguishable_lines`.
- [x] Grep the protocol crate's docs/comments for claims about the change and keep tense honest (LESSON-486). The only claims added are on `PrivacyBlock::cause`, `BlockCause` and `ByteSpan`; `PROTOCOL_VERSION_MAX`'s "version 2" note is about REQ-558's re-typing and stays true unedited, because this addition does not change what shapes this build can read.
- [x] `cargo test -p teton-protocol -p teton -p tetond` green (95 / 201 / 768 tests; clippy clean on all three).

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

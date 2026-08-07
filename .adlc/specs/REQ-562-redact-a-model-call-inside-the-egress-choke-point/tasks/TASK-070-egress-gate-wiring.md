---
id: TASK-070
title: "Wire the RedactionGate into Egress::send and runtime; flip the call-site marker"
status: draft
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: ["TASK-067", "TASK-068", "TASK-069"]
repo: teton-code
---

## Description

The integration task: `RedactionGate` trait + `Egress::with_redaction_gate`
builder; the gate hook in `Egress::send()` after provenance inspection and
before `inner.execute()` (ADR-1); `redact_route()` in runtime.rs naming
`Category::Redact` literally (ADR-3 — deliberately NO taint arm, with the ADR
cited at the resolver); gate construction iff `config.privacy.redact` (ADR-2);
block emission with the TASK-068 causes; `call_sites.rs` flipped to
`Redact => true` with the unreached list now empty.

## Files to Create/Modify

- `crates/tetond/src/egress/mod.rs` — `with_redaction_gate`, the hook in `send()`, `EgressError` variant(s) for redaction blocks
- `crates/tetond/src/egress/redact.rs` — `RedactionGate` trait definition (async scan entry point)
- `crates/tetond/src/runtime.rs` — `redact_route()` beside the five resolvers; `RedactionGateImpl` resolving the route per scan and driving TASK-069's `scan()`; gate installation at every `Egress::new` site behind the config switch
- `crates/tetond/src/call_sites.rs` — `Category::Redact => true`; unreached-list assertion updated
- `crates/tetond/src/harness/duty.rs` — mutation-check table gains the redact rows (documentation of AC-8's mutations and their catching tests)

## Acceptance Criteria

- [ ] Ordering (AC-11): a payload blocked by provenance produces zero scanner calls — asserted by call count on a counting mock gate.
- [ ] Off means off (AC-13): with no `[privacy]` table, a remote turn produces zero scanner calls and no event or report claims a scan ran; enabling the switch and repeating the same turn produces exactly one scan. Both legs by call count.
- [ ] Fail closed (AC-3 path at this layer): gate returning `Unavailable` blocks the send — captured transport records zero requests — and emits `privacy_block` with `ScanUnavailable`.
- [ ] Block on High (AC-1 path at this layer): a High-finding verdict blocks with `Redaction { kind, span }` cause and a non-secret locus `path`; Low-only and Clean verdicts forward, and the forwarded bytes are byte-identical to the input request (AC-9, asserted by capture).
- [ ] `call_sites.rs` passes: the scanner finds `Category::Redact` at `redact_route()`, the marker says reached, the unreached list is empty.
- [ ] Every remote path crosses the gate: a `RemoteDuty` send with the gate installed is scanned too (one test proves a duty-egress payload is subject to the gate).
- [ ] `cargo build --workspace && cargo test --workspace` green (workspace build first — LESSON-489's sibling trap).

## Technical Notes

- The gate hook must not reorder cost metering: metering wraps the response of
  allowed forwards only; blocked sends bill nothing (they never execute), which
  is today's behaviour for boundary blocks — keep it.
- `EgressError`: extend `PrivacyBlocked` or add a sibling variant so the turn
  loop's existing failure sentence path renders the cause; the sentence for
  ScanUnavailable must say "could not run", not "found something" (BR-3).
- The gate's scan call is a local engine call under the duty seam — it must not
  hold locks across `.await` in `send()` beyond what the seam already does, and
  it rides the blocking pool via the seam (ADR-006's E-3 rule comes free if the
  scan goes through `DutyRoute::perform`).
- Session taint: a redaction block flows through the same `PrivacyEventSink`,
  so `TaintingPrivacySink` taints the session — subsequent turns pin local
  (BR-8 stays intact; the sink is cause-agnostic per TASK-068).
- ADR-3's asymmetry (no taint arm in `redact_route`) gets a comment citing the
  ADR so a uniformity-minded reviewer doesn't "fix" it (LESSON-484 corollary).

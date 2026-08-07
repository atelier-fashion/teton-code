---
id: TASK-066
title: "Redaction verdict types, deterministic pattern pass, and the pure decision function"
status: draft
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: []
repo: teton-code
---

## Description

Create `crates/tetond/src/egress/redact.rs`: the data model
(`RedactionVerdict`, `Finding`, `FindingKind`, `Confidence`), the deterministic
credential-pattern pass, the input cap, and the pure
`decide(&RedactionVerdict) -> EgressDecision` function (ADR-4, ADR-6). No
wiring, no model call, no config — this is the foundation the duty and the gate
consume.

## Files to Create/Modify

- `crates/tetond/src/egress/redact.rs` — new module: types, pattern pass, `decide`, `REDACT_INPUT_MAX_BYTES`
- `crates/tetond/src/egress/mod.rs` — `pub mod redact;` declaration only (no behavioural change in this task)

## Acceptance Criteria

- [ ] `RedactionVerdict { outcome: Clean|Findings|Unavailable, findings, scanned }` with the spec's invariants: `findings` non-empty iff `Findings`; `scanned: false` iff `Unavailable`. Enforce by constructor functions, not by discipline.
- [ ] `Finding { kind: FindingKind, span: Range<usize>, confidence: Confidence }` — **no text field exists on the type** (BR-6 is structural, ADR-5).
- [ ] Pattern pass detects the five shapes (`sk-…`, `AKIA…`, `ghp_…`, `Bearer …`, `*_API_KEY/_TOKEN=…`) and yields `Confidence::High` findings with correct byte spans; table-driven tests cover each shape plus a clean payload (non-vacuity: the clean case asserts the pass RAN and found nothing, not that it was skipped).
- [ ] `decide`: any High finding → Block; Low-only → Forward; Clean → Forward; Unavailable → Block. Table-driven over (high, low) × (single finding, mixed findings) per AC-10, including the low-confidence-only-payload-is-not-blocked row.
- [ ] Over-cap input (`len > REDACT_INPUT_MAX_BYTES`, 64 KiB) maps to `Unavailable` — with a test asserting it is Block, never Forward (BR-7).
- [ ] `cargo test -p tetond` green; no clippy warnings.

## Technical Notes

- Pure code only — no async, no engine, no I/O. This is what makes AC-10's
  table-driven tests possible and cheap.
- Spans are byte ranges into the scanned text. Pattern hits derive spans from
  match positions directly.
- Regex crate is already in the workspace dependency tree? Verify; if not, use
  `std` scanning or add the dependency deliberately (spec says "No new crates" —
  prefer hand-rolled matching for the five anchored shapes if `regex` is not
  already a workspace dep).
- Follow LESSON-485: for each decision row, name the discriminating state — a
  fixture whose Block and Forward outcomes coincide pins nothing.

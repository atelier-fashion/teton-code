---
id: TASK-007
title: "tetond egress module: privacy boundary enforcement"
status: complete
parent: REQ-544
created: 2026-07-17
updated: 2026-07-17
dependencies: [TASK-004, TASK-005]
---

## Description

The single egress choke point (D-2): implements `Transport` for real HTTPS,
enforces BR-1 (local-only content never leaves) on every outbound payload,
emits `privacy_block` events, and hosts the hook where TASK-008 records cost.
Delivers AC-5's enforcement + test harness.

## Files to Create/Modify

- `crates/tetond/src/egress/mod.rs` — the only place a real HTTP client is constructed; implements `Transport`; all provider AND MCP traffic flows through here
- `crates/tetond/src/egress/inspector.rs` — payload inspection: tracks boundary-tagged content (by session content provenance, not string scanning) entering outbound requests; blocks or re-routes per boundary mode
- `crates/tetond/src/egress/provenance.rs` — content provenance tagging: file reads under a boundary taint the derived context blocks they enter (summaries, snippets, tool results)
- `crates/tetond/tests/egress_capture.rs` — AC-5 harness: mock-transport capture asserting zero boundary content in any outbound payload across a scripted session; deliberate-violation case emits `privacy_block`

## Acceptance Criteria

- [ ] Scripted session touching `local-only` files completes with zero boundary bytes in captured egress (AC-5)
- [ ] Deliberate attempt to route boundary content remotely → `privacy_block` event with path + provider + action taken
- [ ] Provenance survives derivation: a summary OF a boundary file is itself blocked (BR-1's "derived verbatim" clause)
- [ ] Error reports/telemetry paths verified to exclude boundary content (BR-1)
- [ ] No other module in the workspace constructs an HTTP client (enforced by a CI grep/deny-list check added in this task)

## Technical Notes

Provenance-tagging is the honest implementation of BR-1 — string-matching
egress payloads against file contents is both slow and evadable. Tag at the
context-assembly layer: every context block carries its source set; egress
rejects requests whose source set intersects a local-only boundary. Document
the residual limit (model-generated paraphrase of boundary content routed in a
LATER turn) in the module docs and spec Assumptions at wrapup.

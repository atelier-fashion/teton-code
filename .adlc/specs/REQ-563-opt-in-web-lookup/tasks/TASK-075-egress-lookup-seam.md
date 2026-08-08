---
id: TASK-075
title: "Egress lookup seam: taint/authorship gate, search redaction gate, events + ledger emission"
status: complete
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: ["TASK-072", "TASK-073"]
---

## Description

The choke-point half of the feature (architecture D-2/D-4/D-6): a `lookup()`
entry on the egress module that applies the authorship/taint gate, runs the
always-installed search redaction gate, maps offline errors to the notice
outcome, and emits events + ledger rows. No harness surface yet.

## Files to Create/Modify

- `crates/tetond/src/egress/lookup.rs` — new: `LookupRequest { kind: Fetch { url } | Search { query }, authorship: UserPasted | ModelComposed }`, `LookupOutcome`, and `Egress::lookup(req, ctx) -> LookupOutcome`. Gate order: (1) tier ceiling + allowlist refusals are the CALLER's job (harness) — this seam asserts only egress-side guards; (2) taint gate: `ModelComposed` + `SessionTaint::is_tainted` + no session override → `taint_restricted` (no packet); (3) search redaction gate (below); (4) wire via the existing transport (no redirect following — bounded manual redirect loop for Fetch, ≤3 hops, re-running the caller-supplied per-hop host check); (5) connect/DNS/timeout errors → `offline` outcome, HTTP status errors → distinct error outcome (never a turn error — BUG-152 taxonomy).
- `crates/tetond/src/egress/mod.rs` — expose the lookup entry; hold the search `RedactionGate` slot (installed whenever config tier = Search, independent of `[privacy] redact` — architecture D-6); emit `web_lookup` events through the existing `PrivacyEventSink`-adjacent event path; record `web_lookups` rows after outcome (TASK-073 fn).
- `crates/tetond/src/runtime.rs` — construct the lookup seam with: session-taint handle (existing `SessionTaint`), the session override flag (new session-scoped bool with `web/override` RPC handler — user-only channel), and the search gate built from the SAME composite scanner pieces as `RedactionGateImpl` (runtime.rs:2973-3010).

## Acceptance Criteria

- [x] `deny_http_client` posture unchanged: the lookup path uses the egress-owned transport; no new HTTP client anywhere (compile-time check still passes).
- [x] Taint gate: with a tainted session, `ModelComposed` lookups return `taint_restricted` with zero transport calls (CaptureTransport asserts); `UserPasted` lookups proceed; setting the override flag restores `ModelComposed`; the override is reachable only via the client RPC (no tool-dispatch path calls it).
- [x] Search gate: installed ⇔ tier is Search; every Search query is scanned before wire; a High finding → `blocked_redact`, zero transport calls; verdict `Unavailable` (engine absent/stalled/over-cap) → `blocked_redact` with the unavailable reason — a guard that cannot run is a block, not a skip (LESSON-492); Fetch requests are NOT scanned by this gate (parity clause, spec BR-2).
- [x] Scan input measured on the rendered scan prompt, not the raw query (LESSON-491; reuse REQ-562's cap machinery).
- [x] Offline: connect-level failures yield `offline` outcome; the error carries no query text; a ledger row and `web_lookup` event are still recorded.
- [x] Every outcome (completed and all refusals) records exactly one ledger row and one `web_lookup` event; events/rows carry host only, never query text or full URL (test asserts).

## Technical Notes

- The manual redirect loop must re-validate the NEXT hop's host with the
  caller-supplied check before following (host check is a closure argument so
  the harness can bind allowlist + tier semantics without egress knowing
  config shapes).
- Search request construction: endpoint from config, key resolved from
  keychain by ref AT CALL TIME (daemon resolves — ADR-007 keychain identity),
  key never logged, never in events/ledger.
- `ScanUnavailable` here BLOCKS (unlike the provider path where it also blocks
  but does not taint) — and a web `blocked_redact` must NOT mark session taint:
  taint semantics stay owned by the existing `TaintingPrivacySink` rules;
  document this at the emission site.

---
id: TASK-078
title: "Acceptance suite: egress-capture, consent matrix, taint/override, search-redact, e2e"
status: draft
parent: REQ-563
created: 2026-08-08
updated: 2026-08-08
dependencies: ["TASK-076", "TASK-077"]
---

## Description

The integration/acceptance layer proving the 13 ACs with the repo's existing
fixtures (CaptureTransport, CountingGate, scripted engine, TestDaemon). Unit
tests live in their feature tasks; this task is the cross-piece evidence.

## Files to Create/Modify

- `crates/tetond/tests/web_lookup_egress.rs` — new: CaptureTransport-backed suite: AC-1 (tier Off → a session makes ZERO lookup transport calls; prompt names the opt-in), AC-3 (tainted session ModelComposed → `taint_restricted`, no packet; planted credential in search query → `blocked_redact` with fixtures built through the production encoder — LESSON-490), AC-8 (connect error → offline outcome, turn completes), AC-10 (second lookup served from cache, zero transport calls, `web_cache_hit`… ledger row present), AC-11 (fetch a fixture HTML page; assert no raw page bytes in any captured remote-provider payload — only reduced text).
- `crates/tetond/tests/web_consent_matrix.rs` — new: AC-2 (deny → no packet; allow-once → exactly one; allow-session → until session end, not beyond; enable-permanent → config persisted, next daemon start honors it), AC-4 (tier gradation refusals name the missing tier), AC-9 (allowlist matrix incl. user-pasted exemption), AC-12 (taint trip → visible notice + `UserPasted` proceeds; override via RPC restores; override attempted via tool dispatch fails; fresh session restricted again), AC-13 (search gate installed ⇔ tier Search; Unavailable blocks the query; local tier absent → search not offered at consent time).
- `crates/tetond/src/harness/render.rs` tests (extend in place) — AC-5: a fetched page containing frame markers, role labels, and BOTH envelope spellings is neutralized by the existing sanitizers when framed as a web result; assert the ADR-009 bidirectional coverage tests still pass UNCHANGED (no new markers were introduced — that absence is the assertion).
- `crates/teton/tests/cli_e2e.rs` — extend: `/web allow` + `/web refresh` command flows against TestDaemon with scripted engine; status row shows web state; `/cost` includes lookup lines (AC-6); `/help` lists both commands.

## Acceptance Criteria

- [ ] Every AC (1–13) is exercised by at least one test in this task or a feature task, and a comment header maps AC → test fn name.
- [ ] Egress-capture assertions are byte-level: zero lookup traffic for AC-1, no raw page bytes for AC-11, no query text in any event/ledger row.
- [ ] Redact fixtures are built through the production encoder path (LESSON-490 — no hand-written raw fixtures for encoded forms).
- [ ] The suite runs with the scripted engine harness (no model download, no network) and passes with `cargo test --workspace`.
- [ ] Negative controls: each blocking test proves it can fail (temporarily invert a gate in-test via config, not by editing prod code) — a passing test that has never failed proves nothing (LESSON-479 falsification discipline).

## Technical Notes

- Reuse `CaptureTransport` (egress_capture.rs:44-67), `CountingGate`
  (egress/mod.rs:1002-1030), `TestDaemon` + `TETON_LOCAL_SCRIPT`
  (cli_e2e.rs:65-165). No new test scaffolding.
- AC-13's "local tier absent" leg: run with the loaderless default build state
  (no `llama` feature) where the engine slot is honestly absent.
- Workspace build first, then targeted runs — a stale daemon binary can mask
  failures in e2e (repo memory: targeted e2e runs test a stale daemon).

---
id: TASK-071
title: "E2E acceptance matrix, sentinel grep, mutation checks, and the latency procedure"
status: draft
parent: REQ-562
created: 2026-08-07
updated: 2026-08-07
dependencies: ["TASK-070"]
repo: teton-code
---

## Description

The verification task closing every AC end-to-end through the real daemon
surfaces: the egress-capture acceptance matrix, the AC-6 sentinel grep across
all emitted surfaces, the AC-12 taint short-circuit, the AC-5 engine-backed
capture test, the AC-8 mutation checks with their catching tests, and the AC-7
latency procedure in docs/manual-verification.md.

## Files to Create/Modify

- `crates/tetond/tests/redact_egress.rs` — new integration suite (CaptureTransport + CapturingSink + scripted engine): the acceptance matrix
- `crates/tetond/tests/egress_capture.rs` — extend only if shared fixtures live here; prefer the new file
- `docs/manual-verification.md` — AC-7 latency procedure (budget from ADR-8), recorded NOT RUN
- `.adlc/specs/REQ-562-redact-a-model-call-inside-the-egress-choke-point/requirement.md` — tick ACs as they are proven

## Acceptance Criteria

- [ ] AC-1: planted paraphrased credential, clean provenance, no matching boundary — blocked; `privacy_block` names the redaction cause; captured transport shows zero outbound requests. (Scripted model verdict provides the Low/High hit; use a pattern-shaped sentinel for the High leg and a scripted model catch for the paraphrase leg.)
- [ ] AC-2: clean payload forwards AND the test proves the scan ran (scanner call count == 1), not skipped (non-vacuity — LESSON-485).
- [ ] AC-3: `[privacy] redact = true` with no local tier (llama feature absent / engine unavailable): blocked, captured bytes show nothing sent, cause is ScanUnavailable, and no surface claims `scanned: true`.
- [ ] AC-5: a remote-kind provider registered under the id `local` with redact enabled — the scan never dispatches over HTTP: captured transport records zero scan-originated requests (asserted by capture, not id comparison).
- [ ] AC-12: a tainted session's turn produces zero scanner calls (call count), because the turn never reaches remote egress.
- [ ] AC-4's "no locality guard was added" leg, asserted behaviorally (NOT by a src-text grep — LESSON-489): with a genuinely engine-backed local provider registered under a NON-`local` id, the redact scan still runs and serves. An id-comparison guard would wrongly refuse this fixture; its success is the discriminating evidence the guard does not exist (LESSON-485).
- [ ] AC-6: a distinctive sentinel (e.g. `AKIA_SENTINEL_562_…` shaped to trip the pattern pass) planted; serialize every captured event, daemon log output, and returned error from the run; grep finds zero occurrences of the sentinel outside the payload itself.
- [ ] AC-9: for both Clean-forward and Low-only-forward, captured outbound bytes are byte-for-byte identical to the assembled request.
- [ ] AC-8 mutations, each run with the workspace freshly built (LESSON-489): (a) `decide`'s Unavailable arm → Forward; (b) input cap removed; (c) a text field added to `Finding` and threaded to the event; (d) an id-based locality assertion replacing the capture assertion in AC-5's test. Each names the test that goes red in the duty.rs mutation table; a green mutation is REPORTED in the task completion note, not quietly fixed.
- [ ] AC-7: manual-verification.md gains the latency procedure (real weights, payload at cap, p50/p95 against ADR-8's 2s/5s budget), marked NOT RUN, following the REQ-557/558 entry format.
- [ ] AC-13/AC-14/AC-10/AC-11 already pinned at their layers (TASK-066/067/070) — this task re-runs the full workspace suite and confirms no gap in the AC checklist, ticking requirement.md.
- [ ] `cargo build --workspace && cargo test --workspace` green.

## Technical Notes

- Script format: duty answers are consumed off-script via contract recognition;
  keep the redaction scripted verdicts unambiguous against grep-content
  collisions (runtime.rs DUTY_CONTRACT_PREFIX_BYTES trap).
- The AC-5 fixture is the BUG-156 shape: register a remote provider under the id
  `local`; the discriminating state is that an id-comparison passes while the
  capture assertion fails if the pin ever dispatches remotely (LESSON-485 #5).
- Do not let any test read `src/` as text unless it tolerates concurrent
  modification (LESSON-489 / BUG-159); the call_sites scanner already exists —
  do not add another.
- Mutation (c) intentionally violates the type-level no-text property; it is a
  compile-plus-thread mutation — record the exact diff applied in the mutation
  table so it is reproducible.

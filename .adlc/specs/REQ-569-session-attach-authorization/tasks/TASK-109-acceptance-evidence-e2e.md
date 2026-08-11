---
id: TASK-109
title: "Acceptance evidence: descendant refusal, grant flow, resume, full-suite green"
status: draft
parent: REQ-569
created: 2026-08-11
updated: 2026-08-11
dependencies: ["TASK-107", "TASK-108"]
---

## Description

Turn the spec's AC-1..AC-10 into named tests and tick them off. The
load-bearing one is AC-1: a client driven **from a process that is a descendant
of the daemon** — the shape a tool/MCP child actually has — refused at every
door.

## Files to Create/Modify

- `crates/tetond/tests/attach_authorization.rs` — NEW. Raw-socket suite:
  - **AC-1 (the important one):** the test process asks the daemon to run a shell tool that connects back to the socket and attempts `session/attach`, a `monitor` handshake, and `session/prompt` against another connection's session — i.e. the client runs as a genuine daemon descendant. All three refused (`ATTACH_FORBIDDEN` / handshake refusal), and **no `attach_consent_requested` event is published for any of them** (assert the absence with a positive control in the same test, so it cannot pass by the daemon merely being slow). If driving a real descendant proves impractical in-process, the fallback is to inject the ancestry verdict at the seam — but say so explicitly in the test doc and in your report; do not silently downgrade what AC-1 claims.
  - **AC-5:** knowing a session id (from `session/list`) does not enable attach — refused `NOT_GRANTED`.
  - **AC-4:** monitor without a monitor-scope grant refused; an attach grant for a session does not enable monitor.
  - **AC-8:** every refusal asserted at the raw RPC surface, never through the CLI.
- `crates/tetond/tests/attach_consent.rs` — NEW (or same file): **AC-2** second client attaches through the grant flow and then receives that session's events per REQ-568; **AC-3** resume — create, disconnect the only client, reconnect fresh, attach with one consent step; **AC-6** timeout → denied, `attach_refused`, no residual grant.
- `.adlc/specs/REQ-569-session-attach-authorization/requirement.md` — tick AC-1..AC-10 with the pinning test name beside each (AC-9 → TASK-107's test, AC-10 → TASK-105's, referenced not duplicated). Tick BR-1..BR-10 only where the code genuinely discharges them; leave any that don't and say which.
- Full suite: `cargo build --workspace && cargo test --workspace --no-fail-fast` (build first — the e2e suite must exercise a fresh daemon, not a stale one).

## Acceptance Criteria

- [ ] AC-1..AC-8 each pinned by a named test (AC-9/AC-10 referenced from their owning tasks, not duplicated).
- [ ] **AC-7 regression bar:** the single-client create → prompt → stream flow runs with **zero** new prompts or consent steps — asserted explicitly, because this is the flow every existing user has and a consent step leaking into it would be the worst regression this REQ could ship.
- [ ] Every negative assertion is bounded by a positive control in the same test (the REQ-568 ordering-marker pattern — no sleeps standing in for correctness).
- [ ] Full workspace build + test green; report exact totals.
- [ ] Spec AC/BR checkboxes updated with test names; any BR not genuinely discharged is left unticked and named in the report.

## Technical Notes

- Run with `--no-fail-fast` on any failure so the reported count is a total, not a floor.
- Reuse the REQ-568 `TestClient` raw-NDJSON pattern and the `e2e/harness.rs` daemon-spawn helpers rather than inventing a third harness.
- Session ids are now random (TASK-104) — capture them from `session/create`, never construct them.

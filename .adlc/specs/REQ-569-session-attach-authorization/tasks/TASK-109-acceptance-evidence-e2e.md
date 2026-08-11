---
id: TASK-109
title: "Acceptance evidence: descendant refusal, grant flow, resume, full-suite green"
status: complete
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

- [x] AC-1..AC-8 each pinned by a named test (AC-9/AC-10 referenced from their owning tasks, not duplicated).
  — see the table in `crates/tetond/tests/attach_authorization.rs`'s module docs
  and the per-AC evidence sentences in the spec. New here: AC-1, AC-2's delivery
  half, AC-3's resume half, AC-5, AC-7. Referenced, not duplicated: AC-4, AC-6,
  AC-9, AC-10.
- [x] **AC-7 regression bar:** the single-client create → prompt → stream flow runs with **zero** new prompts or consent steps — asserted explicitly, because this is the flow every existing user has and a consent step leaking into it would be the worst regression this REQ could ship.
  — `the_single_client_create_prompt_stream_flow_asks_for_nothing_new` asserts
  `attach_consent_requested`, `attach_refused` **and** `permission_request` are
  all empty, on a turn whose streamed text is asserted too (so the silence is
  not the silence of a flow that never ran).
- [x] Every negative assertion is bounded by a positive control in the same test (the REQ-568 ordering-marker pattern — no sleeps standing in for correctness).
  — AC-1's "no prompt for the descendant" is bounded by an ordinary client
  drawing one on the same daemon and connection; AC-7's three empty lists by the
  same; AC-2's "did not see the earlier envelope" is decided by `seq`; the
  self-approval negative by the second leg making the same grep succeed. The one
  place a wait was structurally needed — "have the departed connections stopped
  being consent surfaces?" — is answered by the `daemon_lifetime` frame the
  departing guard publishes *after* the surface is released, not by a sleep.
  There is no `sleep` in the file.
- [x] Full workspace build + test green; report exact totals.
- [x] Spec AC/BR checkboxes updated with test names; any BR not genuinely discharged is left unticked and named in the report.
  — BR-6 left unticked, BR-3 and BR-9 marked partial, each with the reason in
  the spec and a summary in a new "Residuals at close" section.

## Technical Notes

- Run with `--no-fail-fast` on any failure so the reported count is a total, not a floor.
- Reuse the REQ-568 `TestClient` raw-NDJSON pattern and the `e2e/harness.rs` daemon-spawn helpers rather than inventing a third harness.
- Session ids are now random (TASK-104) — capture them from `session/create`, never construct them.

## Implementation Notes (as built)

- **AC-1 is a real descendant, not an injected verdict.** The task allowed a
  fallback; it was not used. The daemon is given one scripted turn that calls
  the `shell` tool, and the command it runs re-executes this test binary as a
  probe (`the_daemon_descendant_probe_body`, a `#[test]` that no-ops unless
  `TETON_ATTACH_PROBE_SOCKET` is set). So the connections under test come from
  `tetond` → `sh` → probe. The probe reports its own parent chain and the test
  asserts the daemon's pid is in it, so the fixture's central property is
  checked rather than described. Mutation-checked: forcing
  `DaemonProcess::ancestry_of` to answer `NotDescendant` fails the test.
- **One new file, one new harness accessor, and a new raw client.** The file is
  its own integration binary that includes `e2e/harness.rs` by `#[path]` (for
  `Workspace`/`MockProvider`/`Daemon::spawn`), and adds `Daemon::pid()` to that
  harness — AC-1 cannot be asserted without it. It does **not** use
  `harness::Client`: that one auto-answers consent from a reader thread, and
  these tests have to decide when a user says yes, say no, and watch a prompt
  nobody answers. The `RawClient` here is single-threaded, so the order it saw
  frames in is a fact about the daemon.
- **The self-approval visibility ask (new requirement, not in the original file
  list).** `ConsentRoute::self_approved_by` (pure, table-tested),
  `server::self_approval_line` (bounded and control-character-stripped, the
  REQ-568 monitor-log treatment), and one call site in `handle_attach_consent`
  guarded by `resolved && granted && self_approved_by` — so a decision that
  reached no waiter, a denial, and a peer-approved grant are all excluded.
  Mutation-checked in three directions: dropping the predicate, inverting it,
  and silencing the line each fail a different leg of
  `a_consent_the_requester_granted_itself_is_named_as_such_in_the_daemon_log`.
- **AC-6 is referenced, not re-driven at the wire.** The shipped consent window
  is 30 s and is a fixture knob on an in-process `Daemon`, not an env seam on
  the spawned binary, so an e2e timeout leg would buy a 30-second copy of an
  assertion `server::tests::a_denied_or_timed_out_consent_leaves_the_grant_
  registry_empty` already makes better (it inspects the grant registry).
  `multi_client::knowing_a_session_id_does_not_let_another_connection_attach`
  covers `CONSENT_TIMEOUT` on a real socket under a shortened window.

## Deviations

- **BR-6 is not ticked, and BR-3/BR-9 are marked partial.** The daemon side of
  BR-6 is complete and tested end to end; the client surface is outstanding
  (nothing in `crates/teton` calls `session/attach`), and BR-6's second sentence
  — "a mechanism an ambient background process cannot silently satisfy" — is
  not true of the self-render arm. BR-3 is discharged for daemon descendants and
  for the environment/filesystem vectors but not for an arbitrary same-UID
  process holding the socket path. BR-9's named half is done; the six
  daemon-wide methods TASK-107's audit found are [[BUG-162]]. All three are
  written up in the spec's new "Residuals at close" section.
- **Two ACs gained evidence the task did not ask for**, because the halves were
  genuinely missing rather than merely unreferenced: AC-2's *delivery* half
  (`a_client_that_attached_through_the_grant_flow_receives_that_sessions_events`
  — decided by envelope `seq`, so "it did not see the earlier one" is a
  decision) and AC-3's resume-with-nobody-attached path, which
  `conversation_carry`'s AC-9 test cannot show because its client A is still
  attached when B asks.

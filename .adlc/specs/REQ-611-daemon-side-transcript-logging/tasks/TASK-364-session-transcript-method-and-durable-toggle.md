---
id: TASK-364
title: "session/transcript on the daemon, SetTranscriptEnabled persistence, and the doctor posture"
status: draft
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-363]
---

## Description

The two switches on the daemon side. `session/transcript` (ADR-6) flips or reads the sink's
per-session state and publishes `transcript_state`; `ConfigUpdate::SetTranscriptEnabled` (ADR-5)
persists through `config_document` behind `config/set`'s existing gates; `config/get` exposes
the posture for `doctor`. Covers BR-2 (both lifetimes), BR-3 (no model reach), BR-6 (status
reports degraded), BR-9's `dir_refused` arm, BR-15, BR-16, and the daemon halves of AC-3, AC-4,
AC-5, AC-6, AC-7, AC-10, AC-11.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — `handle_session_transcript` modelled on
  `handle_session_permissions` (line ~3073): `refuse_unmintable_session_id`, `may_drive`, then
  `runtime.session_transcript`; dispatch arm beside `SessionClearParams::METHOD` (line ~2460).
  `handle_config_set` gains the `SetTranscriptEnabled` arm in the `registered` match (line
  ~3320) — no new gate, no exemption.
- `crates/tetond/src/runtime/mod.rs` — `session_transcript(&params, &events) ->
  SessionTranscriptResult`: `On` opens or resumes via the sink (refusing with `dir_refused` when
  the effective dir cannot be created or exists wider than `0o700`), `Off` closes with `session_command`, `Status`
  reads; each state change publishes `transcript_state` session-scoped. Persist
  `SetTranscriptEnabled` through the same path `SetEffort` uses. `config/get`'s snapshot gains
  `transcript: { enabled, dir, retain_days }`.
- `crates/tetond/src/runtime/config_document.rs` — render/update the `[transcript]` table.
- `crates/tetond/src/transcript/mod.rs` — `set_enabled(session, bool, reason) -> Result<Status,
  Refused>`.
- `crates/teton-protocol/src/methods.rs` — `ConfigSnapshot` gains the transcript posture.

## Acceptance Criteria

- [ ] AC-3 (daemon half): `On` for a session created under `enabled = false` opens a file, the
      pre-switch conversation is absent from it, and a `transcript_state { true,
      session_command }` reaches the attached connection; `config.toml` bytes are identical
      before and after (read the file, do not infer — LESSON-519).
- [ ] AC-4 (daemon half): `Off` writes `transcript_closed { session_command }` and publishes
      `transcript_state { false }`; a following prompt adds no line; `On` again appends
      `transcript_resumed` to the **same** file and `n` continues.
- [ ] AC-5 (daemon half): `Status` returns enabled/path/records/degraded **as the RPC response**;
      no frame carrying the path reaches a second attached connection or a monitor (assert on raw
      wire text, as REQ-569 BR-10's test does).
- [ ] AC-7 / BR-3: `session/transcript` from a connection not attached to the session is refused
      `NOT_ATTACHED`; there is no tool named or aliasing `transcript` in the `ToolRegistry`
      (assert by enumerating the registry, not by attempting a call).
- [ ] AC-6 / BR-16: `SetTranscriptEnabled(true)` via `config/set` on an attested seam writes
      `enabled = true` under `[transcript]` — read back and re-parse the file; the refused
      counterpart (`TETON_PRESENCE_ACCEPT=fail` seam, LESSON-519) on the same fixture leaves the
      bytes identical; a session created afterwards records and one created before does not
      change state. Use a payload that would persist if the gate were bypassed (LESSON-520).
- [ ] AC-10 (daemon half): with the dir made unwritable by the test seam, the next write yields
      one `transcript_state { false, write_failure }`, `Status` reports `degraded`, and the
      in-flight turn returns normally with no further write attempts.
- [ ] AC-11: a `dir` that cannot be created, or that exists `0o755`, → `transcript_state { false,
      dir_refused }` and the session runs without a transcript; a fresh `dir` inside the session
      root is accepted and opens (benign path — the read refusal is TASK-368's, not this one's).
- [ ] BR-2: a durable change while a session runs leaves that session's effective state
      untouched.
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/tests/transcript.rs::a_durable_change_applies_to_later_sessions_only` | yes |
| BR-3 | test-case | `crates/tetond/tests/transcript.rs::no_tool_reaches_the_transcript_toggle_and_an_unattached_connection_is_refused` | yes |
| BR-6 | test-case | `crates/tetond/tests/transcript.rs::write_failure_announces_once_degrades_status_and_spares_the_turn` | yes |
| BR-9 | test-case | `crates/tetond/tests/transcript.rs::an_uncreatable_or_wide_dir_is_refused_and_a_fresh_in_root_dir_opens` | yes |
| BR-15 | test-case | `crates/tetond/tests/transcript.rs::status_answers_the_asker_and_the_state_event_carries_no_path` | yes |
| BR-16 | test-case | `crates/tetond/tests/config_set_attestation.rs::set_transcript_enabled_writes_on_accept_and_nothing_on_refuse` | yes |
| AC-3 | test-case | `crates/tetond/tests/transcript.rs::on_records_from_the_switch_forward_and_writes_no_config` | yes |
| AC-4 | test-case | `crates/tetond/tests/transcript.rs::off_closes_and_on_resumes_the_same_file` | no |
| AC-5 | test-case | `crates/tetond/tests/transcript.rs::status_answers_the_asker_and_the_state_event_carries_no_path` | yes |
| AC-6 | test-case | `crates/tetond/tests/config_set_attestation.rs::set_transcript_enabled_writes_on_accept_and_nothing_on_refuse` | yes |
| AC-7 | test-case | `crates/tetond/tests/transcript.rs::no_tool_reaches_the_transcript_toggle_and_an_unattached_connection_is_refused` | yes |
| AC-10 | test-case | `crates/tetond/tests/transcript.rs::write_failure_announces_once_degrades_status_and_spares_the_turn` | yes |
| AC-11 | test-case | `crates/tetond/tests/transcript.rs::an_uncreatable_or_wide_dir_is_refused_and_a_fresh_in_root_dir_opens` | yes |

## Technical Notes

`Status` uses `may_drive`, not `may_receive`, on purpose (ADR-6): a monitor may see the state
event but must not learn the path. Do not "relax" this for convenience.

A `dir` inside the session root is allowed (spec AC-11 as validated): the jail denial in
TASK-368 is what keeps it unreadable, and `/cd` therefore needs no re-check here.

`config/set` already runs `refuse_unattested_commitment` before parsing. Add the variant to the
`registered` match and nothing else; the attestation test file already has the accept/refuse
seam pair to copy.

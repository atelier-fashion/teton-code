---
id: TASK-363
title: "The bus tap, the turn-path hand-offs, session lifecycle, and shutdown flush"
status: draft
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-361, TASK-362]
---

## Description

Makes the sink live: `EventBus` gains the tap (ADR-1), `SessionEvents` carries the sink and hands
it the prompt, tool input and tool result, `handle_permission_respond` hands it the decision,
the registry tells it when sessions start and end, and daemon teardown flushes. After this task
a session with `[transcript] enabled = true` produces a complete file; there is no session
command yet. Covers BR-2 (config default at creation), BR-4, BR-5 (tap half), BR-7 (daemon-scoped
excluded), BR-10, BR-11, AC-2, AC-8, AC-18.

## Files to Create/Modify

- `crates/tetond/src/broadcast.rs` — `pub trait EventTap: Send + Sync { fn observe(&self,
  envelope: &EventEnvelope); }`, `EventBus::install_tap(&self, Arc<dyn EventTap>)`, the call in
  `publish` after `seq` is minted and before `subscribers.retain`. Only envelopes with
  `session_id.is_some()` are offered to the tap (BR-7).
- `crates/tetond/src/transcript/mod.rs` — `impl EventTap for TranscriptSink` (a `try_send`; on
  `Full` bump the session's dropped counter).
- `crates/tetond/src/harness/turn_loop.rs` — `SessionEvents { bus, session_id, sink:
  Option<Arc<TranscriptSink>> }`; new methods `prompt_submitted(turn_id, &[PromptBlock],
  skill)`, `tool_input(id, name, &Value)`, `tool_result(id, status, &str)`, each a `sink.record`
  and nothing else. Call sites: the turn's entry for the prompt; `serve_tool_call` where `name`
  and `arguments` are in hand; `run_the_allowed_tool` at the point the folded result exists
  (before `frame_untrusted_builtin` — record what the tool returned, not the frame).
- `crates/tetond/src/runtime/turn.rs` — construct `SessionEvents` with the runtime's sink at
  lines ~877 and ~3121.
- `crates/tetond/src/server.rs` — in `handle_permission_respond`, after `resolve_from` succeeds,
  `sink.record(session, permission_decided { request_id, option_id, remembered })`.
- `crates/tetond/src/sessions.rs` — `SessionRegistry::create` → `sink.session_created(id, root,
  config.transcript.enabled)`; removal → `sink.session_closed(id, SessionEnded)`.
- `crates/tetond/src/runtime/mod.rs` — build the sink in `from_env` from
  `config.transcript` and `resolve_data_dir`; call `prune` once at
  start and log the count on stderr when non-zero; hold `Arc<TranscriptSink>`.
- `crates/tetond/src/main.rs` / `lifetime.rs` — on orderly teardown, `sink.shutdown().await`
  which closes every open file with `daemon_shutdown` and flushes before the socket is removed.

## Acceptance Criteria

- [ ] BR-5 (tap half): a unit test fills a 1-slot sink channel, publishes 100 envelopes, and
      asserts `publish` returned each time and the bus's ordinary subscribers still received all
      100; the sink's dropped counter equals the shortfall.
- [ ] ADR-1: a subscriber evicted for lag does not affect the tap — after eviction the tap still
      observes every envelope.
- [ ] BR-7: a daemon-scoped envelope (`session_id = None`) is never offered to the tap; asserted
      by publishing `model_lifecycle` and checking the tap saw nothing.
- [ ] BR-4: `grep -n 'publish' crates/tetond/src/harness/turn_loop.rs` after the change shows no
      publish carrying `PromptBlock`, tool arguments, or a tool result — recorded as a structural
      assertion in the transcript integration test (source-scanning check bounded to the function
      bodies that gained hand-offs, per `conventions.md`).
- [ ] BR-2: a session created while `enabled = true` opens a file before its first prompt; one
      created while `false` opens nothing (AC-1's filesystem inspection).
- [ ] AC-2: end-to-end against `MockProvider`, the file contains the record kinds the spec lists
      with the three order relations and contiguous `n`.
- [ ] BR-10: the `permission_decided` record and the `session_grant_minted` handling carry no
      secret; the existing wire forms are recorded verbatim.
- [ ] BR-11: no call into `harness/redact.rs` from the transcript module or its hand-offs
      (structural: `grep -c redact crates/tetond/src/transcript` is 0).
- [ ] AC-8: two concurrent sessions, one event each, two files, no cross-talk; no daemon-scoped
      event in either file.
- [ ] AC-18: orderly shutdown of a daemon with an open transcript leaves `transcript_closed {
      daemon_shutdown }` as the last line; `SIGKILL` leaves at most one partial trailing line.
- [ ] `cargo test --workspace --no-fail-fast` is green; grep the output for `FAILED`.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/tetond/tests/transcript.rs::a_session_created_under_enabled_true_opens_a_file_and_under_false_opens_nothing` | yes |
| BR-4 | test-case | `crates/tetond/tests/transcript.rs::no_turn_loop_publish_carries_prompt_or_tool_content` | no |
| BR-5 | test-case | `crates/tetond/src/broadcast.rs::the_tap_never_blocks_publish_and_counts_its_drops` | yes |
| BR-7 | test-case | `crates/tetond/src/broadcast.rs::daemon_scoped_envelopes_are_not_offered_to_the_tap` | yes |
| BR-10 | test-case | `crates/tetond/tests/transcript.rs::permission_decided_and_grant_records_carry_no_secret` | no |
| BR-11 | test-case | `crates/tetond/tests/transcript.rs::the_transcript_module_never_calls_the_redactor` | no |
| AC-2 | test-case | `crates/tetond/tests/transcript.rs::one_prompt_one_tool_call_yields_a_complete_file` | no |
| AC-8 | test-case | `crates/tetond/tests/transcript.rs::two_sessions_never_share_a_file_and_daemon_events_appear_in_neither` | yes |
| AC-18 | test-case | `crates/tetond/tests/transcript.rs::orderly_shutdown_closes_the_file_and_sigkill_leaves_one_partial_line` | no |

## Technical Notes

`observe` runs under the bus mutex. It must be `try_send` and nothing else — no allocation
beyond the clone, no logging. The test that fills the channel is the guard; write its mutation
(make `observe` `blocking_send`) and record that the test hangs/fails.

Record the tool result **before** `frame_untrusted_builtin` so the file holds what the tool
returned; the frame is a model-facing artifact, not part of the session's surface. Record the
prompt at the turn's entry, after the claim (LESSON-539: claim first, then read).

`SessionEvents::new` keeps its signature; add `with_sink`. Test constructors at turn_loop.rs
~3340/3420/3529 pass `None`, so the harness unit suite does not write files.

The shutdown path must not race the writer task: `shutdown()` sends a close for every session
then awaits the task's join with a bounded timeout, and only then does `main` remove the socket.

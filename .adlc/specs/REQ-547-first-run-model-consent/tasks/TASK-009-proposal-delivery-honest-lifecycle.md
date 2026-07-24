---
id: TASK-009
title: "Make the proposal deliverable and nameable; stop the lifecycle claiming untruths"
status: complete
parent: REQ-547
created: 2026-07-21
updated: 2026-07-23
dependencies: [TASK-004, TASK-007]
---

## Description

TASK-008 verified two defects that negate this REQ's premise:

1. **The proposal is structurally undeliverable and unnameable.** `tetond::main`
   publishes `model_selection_proposed` before `server::serve` accepts its first
   connection, so no client ever receives it. Clients fall back to
   `model/status.pending_request_id` + `model/list`, but neither can say WHICH
   entry was proposed — the CLI prints "the daemon's own pick for the small
   band". BR-2 requires naming the proposed model with its size and RAM floor.
2. **The startup lifecycle asserts untruths.** The synthetic sequence emits
   `download …`, `benchmark …`, `local model … ready` on every client attach —
   including before the user has answered and on a machine with no weights. A
   feature whose thesis is legibility must not lie about its own state.

## Files to Create/Modify

- `crates/teton-protocol/src/methods.rs` — carry the full outstanding proposal (probe report + proposed entry + alternatives + required disk) on the status/pending path so a late-attaching client can render exactly what the event would have shown
- `crates/tetond/src/model_consent.rs` — expose the outstanding proposal payload for that path
- `crates/tetond/src/server.rs` — serve it
- `crates/tetond/src/runtime.rs` — the startup `model_lifecycle` sequence must emit only TRUE stages: no `download`/`benchmark`/`ready` when nothing was downloaded, benchmarked, or is ready; an undecided machine reports awaiting-decision, a declined one reports the local tier absent
- `crates/teton/src/{model_ui.rs, firstrun.rs, client.rs}` — render the named proposal on the late-attach path identically to the live-event path; keep the existing `claim_model_proposal` de-dup so a client seeing both prompts once
- `crates/tetond/tests/e2e/consent_matrix.rs` — un-ignore `ac1_proposal_event_reaches_an_attached_client`; assert the client renders the proposed model BY NAME with size and RAM floor

## Acceptance Criteria

- [x] A client attaching at any time (before, during, or after the proposal is raised) can render the proposal naming the proposed entry, its size, and its RAM floor — AC-1/BR-2 satisfied over a real socket (`consent_matrix::ac1_proposal_event_reaches_an_attached_client` for the daemon half; `teton`'s `cli_e2e::teton_renders_the_first_run_proposal_and_accepts_it_interactively` for the shipped CLI's rendering)
- [x] The previously-ignored delivery test is un-ignored and passes
- [x] A client that both receives the event and polls status prompts exactly ONCE (`teton`'s `client::tests::a_proposal_seen_as_an_event_and_on_model_status_prompts_exactly_once`, over a real Unix socket)
- [x] No `model_lifecycle` stage claims a download, benchmark, or readiness that did not occur — `consent_matrix::the_startup_lifecycle_claims_only_what_actually_happened` asserts undecided-no-weights, declined, and installed
- [x] AC-1's checkbox in requirement.md is checked; AC-2 deliberately left unchecked (no `LlamaEngine` in `tetond` — REQ-544 debt, out of scope here)
- [x] Full workspace green: 685 passed, 0 failed, 1 ignored (the `--features live` smoke test); clippy `-D warnings` and `cargo fmt --check` clean

## Technical Notes

Do NOT fix this by awaiting the server before proposing — the flow is
deliberately spawned beside `serve` so a proposal never blocks the socket. The
right shape is to make the outstanding proposal *retrievable*, so delivery is
not dependent on attach timing at all. Leave AC-2 (no `LlamaEngine` in tetond)
alone — that is REQ-544 debt and a separate scope decision.

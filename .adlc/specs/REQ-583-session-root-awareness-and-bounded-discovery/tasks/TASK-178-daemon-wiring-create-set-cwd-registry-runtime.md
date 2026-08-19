---
id: TASK-178
title: "Daemon wiring: create/set_cwd return the root, the per-turn probe feeds jail and prompt, session/set_cwd moves a live session and clears it"
status: complete
parent: REQ-583
created: 2026-08-18
updated: 2026-08-18
dependencies: ["TASK-174", "TASK-175"]
---

## Description

Wire the derived root through the daemon (ADR-1, ADR-4): `session/create`
returns it, every turn probes it into `ToolContext::for_root` and
`HarnessConfig.session_root`, and the new `session/set_cwd` moves a live
session's root, clears its conversation, and publishes both events. **File
ownership (parallel tier):** `server.rs`, `runtime.rs`, `sessions.rs`,
`crates/tetond/tests/e2e/**`, plus a new `crates/tetond/tests/e2e/session_root.rs`.
Nothing under `harness/` (the `no_tool_can_clear_a_session…` scan forbids the
identifiers there), nothing in `crates/teton`.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — extract the cwd validator from `handle_session_create` (L3211-3226) into `fn validate_session_cwd(cwd: &Path) -> Result<(), String>` whose refusals **name the path** (``cwd `{p}` must be an absolute path``, ``cwd `{p}` does not exist or is not a directory``); `handle_session_create` calls it and, on success, puts `root: Some(session_root::probe(cwd_or_fallback, home))` on `SessionCreateResult` (fallback = the daemon's `repo_root` when no cwd — the same value the turn will jail to); new `handle_session_set_cwd` beside `handle_session_clear` (L4050-4068): parse → `refuse_unmintable_session_id` → `conn.may_drive` → `runtime.set_session_cwd(...)` → `ok`/`error_from`; register `SessionSetCwdParams::METHOD` in the sync dispatch (L2343 area, with the same "no human, no network" comment); add the method to `an_unmintable_session_id_is_refused_by_every_driving_method_before_the_gate` (L9707-9743) and a `dispatch_routes_session_set_cwd_…` test cloned from `dispatch_routes_session_clear_and_tells_attached_from_unattached` (L7088-7156, monitor cannot drive L7238).
- `crates/tetond/src/runtime.rs` — `run_prompt_turn` (L2838-2853): `let root_path = session_cwd.as_deref().unwrap_or(&self.repo_root); let root = crate::session_root::probe(root_path, home()); let tool_ctx = ToolContext::for_root(root_path.to_path_buf(), &root); route.harness.session_root = Some(root);` (keep the BUG-147 comment; `home()` = `std::env::var_os("HOME")`); new `pub fn set_session_cwd(&self, params: SessionSetCwdParams, ...) -> Result<SessionSetCwdResult, RpcErr>` beside `clear_session` (L3104-3132): claim via `try_begin_turn(id, "cd-{n}")` (→ `SESSION_BUSY`/`UNKNOWN_SESSION` through `refused_claim_error`), validate via the extracted validator (→ `INVALID_PARAMS`), read the previous display (probe of the old cwd), `sessions.set_cwd`, `clear_conversation` → `blocks_dropped`, publish `Event::ContextCleared { blocks_dropped }` then `Event::SessionRootChanged { previous_display, root: new_root }` — both `Some(session_id)` — then return `{ root, blocks_dropped }`. Refusal leaves cwd and conversation untouched (validate before mutate).
- `crates/tetond/src/sessions.rs` — `pub fn set_cwd(&self, id: &SessionId, cwd: PathBuf) -> bool` (the `set_title` shape, L552-568); test beside `a_session_remembers_its_cwd` (L820).
- `crates/tetond/tests/e2e/harness.rs` — `create_session_at(cwd: &Path)` helper beside `create_session` (L1066-1076); the existing helper unchanged.
- `crates/tetond/tests/e2e/session_root.rs` — NEW (`mod` it in the e2e entry file). AC-10 daemon half at **every** `PermissionLevel` (iterate `PermissionLevel::ALL`, set via `session/permissions`): `session/create` at `<tmp>/repo` returns `root.kind == project`; `session/set_cwd` to `<tmp>/other` answers `{root: plain, blocks_dropped}` and the socket sees `context_cleared` **then** `session_root_changed` **before** the response (ordering rule); a `read` that succeeded under the old root now fails with the BR-2 shape; `set_cwd` to `/nope` → `INVALID_PARAMS` naming the path, and a following `read` proves the root did not move and the conversation was not cleared; `set_cwd` from a non-driving (monitor) connection → refused; the tools' `ToolContext` root display appears in a jail refusal (integration of TASK-175 through the runtime).
- Also assert AC-9's daemon half: `session/create` with `cwd: /nope` refuses with the path in the message.

## Acceptance Criteria

- [x] `cargo test -p tetond` for `server`, `runtime`, `sessions` and `--test e2e session_root` green; the unmintable-id sweep and the dispatch-routing test cover `session/set_cwd`.
- [x] `SessionCreateResult.root` populated for a cwd'd session and for a no-cwd (fallback) session.
- [x] Every turn's `HarnessConfig.session_root` is `Some(...)` derived from the session's current cwd (assert via an e2e that reads the prompt through the existing capture seam, or a unit test on the assembled `route.harness`).
- [x] `no_tool_can_clear_a_session_and_no_mcp_wiring_path_could` still green (no new identifiers under `harness/`).
- [x] Refusal messages name the path (BR-6); events precede the response.

## Technical Notes

- Home for the probe: `std::env::var_os("HOME").map(PathBuf::from)` read at the call site (the daemon runs as the user).
- `previous_display` is what the CLI's line uses for "moved from"; compute it before mutation.
- Keep the claim held across validate+mutate+clear+publish, released after (mirror `clear_session`).
- Commit as `feat(daemon): the session root rides create/set_cwd and every turn; /cd moves and clears a live session [TASK-178]`.

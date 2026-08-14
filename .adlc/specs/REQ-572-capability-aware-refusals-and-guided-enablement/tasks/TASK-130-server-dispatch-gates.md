---
id: TASK-130
title: "Server: dispatch handlers with may_drive gates and rejection events"
status: complete
parent: REQ-572
created: 2026-08-13
updated: 2026-08-13
dependencies: ["TASK-129"]
---

## Description

Wire the three setup methods into `server.rs` dispatch, gated exactly like
`web/override` (`conn.may_drive(&session_id)`), with the BR-4 defense-in-depth
rejection: a gated-out `preview`/`commit` answers `NOT_ATTACHED` **and**
publishes `WebSetupRejected { origin }` so the refusal is visible in front of
the user (LESSON-505), not only in the RPC error the attacker received.

## Files to Create/Modify

- `crates/tetond/src/server.rs` — `handle_web_setup_plan`, `handle_web_setup_preview`, `handle_web_setup_commit` following `handle_web_override` (server.rs:2123): parse params, `may_drive` check, delegate to the runtime methods, map errors. The rejection event publishes from the gate-failure arm of preview and commit (plan is read-only: gate it, no event).
- `crates/tetond/src/server.rs` — dispatch table entries for the three method names.
- `crates/tetond/src/server.rs` — unit/integration tests in the crate's existing server-test pattern: a second connection (attached to a different session / not attached) calling commit gets `NOT_ATTACHED` and the owning session's subscribers see `WebSetupRejected`; a mutation check note — the may_drive predicate call in the commit arm gets a dedicated test that fails when the check is deleted (LESSON-508: the seam is redundant with the structural no-model-path property, which is exactly why it needs its own test).

## Acceptance Criteria

- [x] All three methods dispatch and answer over a real socket in the existing e2e harness style
- [x] Commit from a non-driving connection: `NOT_ATTACHED` error + `WebSetupRejected` event delivered to the session's subscriber — asserted at a client, not via logs
- [x] Deleting the `may_drive` check in the commit handler fails a named test (comment in the test states why it exists — LESSON-508 rule 2)
- [x] No new daemon-wide method: none of the three appears in `refuse_daemon_wide`'s list (they are session-scoped)

## Technical Notes

Request/response only — these are not consent prompts, so `PendingConsents`
id minting is NOT involved (architecture ADR-1 removed the pending-state
surface). `origin` in the rejection event is a coarse string ("connection
without session access") — never connection internals.

## Implementation notes (as built)

**Surfaces added.** `crates/tetond/src/server.rs`: `handle_web_setup_plan`,
`handle_web_setup_preview`, `handle_web_setup_commit`, the shared
`refuse_setup_without_session_access`, the `SETUP_REJECTED_ORIGIN` constant, and
three `dispatch` arms beside `web/override`. No other file's behaviour changed;
`crates/tetond/tests/multi_client.rs` gained the two socket-level tests.

Deviations from the letter of this file, each with its reason:

1. **The gate-and-announce is one helper called per handler, not two copies.**
   `refuse_setup_without_session_access` follows `refuse_daemon_wide`'s shape
   exactly, and for its stated reason: the *call line* is per method, so the
   mutation check can delete one method's check (LESSON-502/508) while the
   payload the two refusals publish cannot drift apart. Both mutations were run
   and both bite — deleting the line from the commit arm fails
   `a_commit_without_session_access_is_refused_and_the_session_is_told` (and two
   neighbours); deleting it from the preview arm fails
   `a_preview_without_session_access_is_refused_and_the_session_is_told` and
   **nothing else**, which is the LESSON-502 property the second test exists for.
   Deleting only the `publish` (keeping the refusal) fails the socket test
   `a_setup_commit_without_session_access_is_refused_and_the_owner_is_told`.
2. **The plan's silence is asserted, not merely coded.**
   `a_refused_plan_is_silent_while_a_refused_commit_is_not` pins both halves
   together, and bounds the absence by *ordering* rather than a timer: the same
   connection's commit on the same subscription then does publish one, so an
   empty drain cannot be a dead subscription. The reason a refused *read* stays
   silent is written down in the handler: a notice any same-UID peer could raise
   on demand is a notice users learn to skip past, which would cost the commit's
   rejection the attention BR-4 exists to buy it.
3. **AC-1's socket test asserts routing, gating and two real answers — not a
   successful write.** An in-process fixture `Daemon::new()` runs
   `DaemonRuntime::minimal()`, whose `config_path` is `None`, so its commit
   answers `CONFIG_REJECTED` ("this daemon has no configuration file to write").
   `plan` and `preview` return their real derivations (the plan reports
   `search_available: false` and names the gap, the preview returns the `[web]`
   bytes), and the commit is asserted routed and past the gate. The write, the
   validate-then-swap and `web_setup_completed` are pinned where they live, in
   `runtime::tests`' on-disk fixtures (TASK-129) — reaching them from an
   integration test would mean either a public config-path constructor this REQ
   does not need or a process-global `TETON_CONFIG` mutation that the rest of
   the suite in that binary shares. Recorded because it is a judgement, not an
   oversight.
4. **AC-4 is asserted twice.** The enumeration half sweeps
   `daemon_wide_methods()` for the three names; the behavioural half drives a
   `Descendant`/`Indeterminate` connection at each method and requires
   `NOT_ATTACHED` — the same refusal any unattached peer draws — so a future
   swap to the ancestry gate, or a dropped dispatch arm (`METHOD_NOT_FOUND`),
   fails it.
5. **The origin string is pinned as a *kind*** in both the unit and the socket
   test: equal to the constant, and containing no digit — a pid or a connection
   id is what would put a number in it. That is the checkable form of "no
   connection internals in an event payload".

**Suite state at commit.** `cargo test -p tetond --lib`: 1236 passed, 0 failed.
`cargo test -p tetond --test multi_client`: 20 passed, 0 failed.
`cargo check --workspace` clean; `cargo clippy -p tetond --all-targets` clean.
`cargo fmt --check` is clean for both files this task touched; it still reports
one pre-existing wrap in `crates/tetond/src/runtime.rs:2357`, which arrived with
TASK-131's `f9a9493` and is not touched here.

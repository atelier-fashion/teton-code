---
id: TASK-203
title: "The registry has a lifetime: built at session create, rebuilt at /cd, queryable over the wire"
status: complete
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-195, TASK-196]
---

## Description

Where the `SkillRegistry` lives and when it is rebuilt, plus the `skills/list`
handler the CLI's snapshot reads (ADR-1, ADR-2).

## Files to Create/Modify

- `crates/tetond/src/sessions.rs` — the per-session registry, built at create, rebuilt at `set_cwd`
- `crates/tetond/src/server.rs` — the `skills/list` dispatch arm and handler; the `session/set_cwd` rebuild
- `crates/tetond/tests/skills_list_contracts.rs` — the RPC contract suite

## Acceptance Criteria

- [ ] `skills/list` is session-scoped and joins the enumerated method list in `server.rs:9943 an_unmintable_session_id_is_refused_by_every_setup_method_before_anything_else` — **both** enumerations (`:9949` and `:9974`).
- [ ] The registry is built from the session's **probed** root (`DaemonRuntime::session_root_for`, `runtime.rs:3291`), which carries the path and the `RootKind` the `home`-root de-dup needs. It is rebuilt on `session/set_cwd` and the result is visible to a subsequent `skills/list` without a restart (AC-14).
- [ ] `TASK-201`'s `drop_project_skill_grants` is called as part of the `/cd` rebuild, in the same place — a rebuilt registry and a stale project grant must not be able to exist at once.
- [ ] `SkillView.description` and `.argument_hint` are bounded with `bounded_field` **before** they go on the wire. A description with control characters, bidi marks or 4,000 chars reaches the client already bounded and neutralized (BR-3, LESSON-517).
- [ ] Multi-client: two clients attached to one session both see the same registry, and a `/cd` driven by one is visible to the other after its own `skills/list` (copy `crates/tetond/tests/multi_client.rs:1502`).
- [ ] Discovery cost is paid at create and at `/cd`, **not** per turn — asserted through the `RecordingFs` seam from TASK-195 (a two-turn session records the four listings once, not twice).
- [ ] Mutation: skipping the `set_cwd` rebuild fails the AC-14 contract test.

## Technical Notes

- The registry is a snapshot, not a live view. There is no watcher (OQ-3); `/skills reload` is Deferred.
- `session_root_changed` already reaches the client *before* the `session/set_cwd` response (`runtime.rs:3329-3337`), which is the event TASK-207's snapshot refresh hangs off — do not add a second notification.

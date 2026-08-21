---
id: TASK-217
title: "Wire the registry, the gate and the invoker into the tool — the seam whose absence is silent"
status: draft
parent: REQ-587
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-215, TASK-216]
---

## Description

ADR-3. `build_tools` gains what the tool needs, and the addressee finally
reaches a consent raised from inside the loop.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `build_tools` gains `Arc<SkillRegistry>` and `invoker: Option<ConnectionId>`; the call site passes both
- `crates/tetond/src/server.rs` — `invoker` already exists at the reader loop; confirm it reaches `build_tools` too
- `crates/tetond/tests/skill_turn.rs` — the two source-scans this task's signature change breaks (`run_prompt_turn_body()`, `settle_dynamic_context_body()`), widened deliberately

## Acceptance Criteria

- [ ] `build_tools` calls `register_skill_tool` after the built-ins (TASK-216 shipped the function; this task owns the call site and the two new parameters).
- [ ] `build_tools` takes the session's `Arc<SkillRegistry>` — the **same snapshot** `accept_invocation` already reads, taken once per turn. Do not re-read it inside the tool: `discovery_is_paid_at_create_and_at_cd_and_never_per_turn` pins that the registry is a snapshot, and one turn / one snapshot is what makes the roster and the resolution provably the same value.
- [ ] `build_tools` takes `invoker: Option<ConnectionId>` and hands it to `SkillTool`. `ConnectionId` is `Copy`, so the existing consumption at `runtime.rs:~3546` is unaffected — but the parameter must be threaded **past** that line in source order, which today it is not.
- [ ] **The assertion that matters: an addressable connection reached `authorize_skill` from inside the loop.** Not from a fixture that invents a `ConnectionId` — `skill_consent_matrix.rs` does exactly that and would pass either way. Drive a model-issued call and assert the `Consent` double recorded the *invoking* connection.
- [ ] The failure this guards is silent: without the threading, `authorize_skill` takes the `None => Unanswerable` arm and produces placeholders **byte-identical** to REQ-585's tested piped-refusal path, with no test failing, because that arm is already shipped and tested behaviour for an internal caller. A green suite is not evidence here.
- [ ] `ToolContext` is **not** touched. It is the jail type — `repo_root`, `display`, `kind`, `walk` — and dozens of fixtures construct it. `PermissionGate` is not touched either: it is per **session**, so a connection stored on it is whichever connection created the session, not the one that submitted this turn.
- [ ] **BR-5's digest is enforced at the door but nothing mints it yet — wire the user path here.** TASK-215 shipped `skill_grant_key(source, skill, commands, ArgumentInterpolation)`; the gate accepts either spelling and pins whichever it is given, so a caller that keeps minting the plain key **silently keeps REQ-585's behaviour with nothing red**. `runtime.rs:~2995` mints `permission_key: skill.permission_key()` at `accept_invocation` and `~:3080` spends it. Whether a body interpolated `$ARGUMENTS`/`$N` is the *expander's* fact — after substitution a command carries no trace of it — so the interpolation verdict has to travel from `expand` to the mint. Assert it behaviourally: two user-typed invocations of one skill with **different** arguments do not share an answer.
- [ ] **`drop_project_skill_grants` is narrower in name than in effect.** TASK-215 widened it to retain on `expires_on_session_root_change` (skill grants *and* the acknowledgment) but could not rename across an owner boundary — `DaemonRuntime::drop_project_skill_grants` and a `server.rs` doc link are your files. Rename it to match what it does, or leave the name and say why in the doc.
- [ ] **BR-9 asks the client to render three facts no event carries.** TASK-219 found this and could not fix it from the client: `SkillInvoked` has no shadowing fact, no flags and no per-turn count, and `render_event` sees only `SessionState` — the registry snapshot lives on `UiContext` — so the client cannot derive any of them either. BR-9 requires the echo line to name shadowing (`skill validate (project — shadows your user skill, …)`) and `/verbose` to add *the flags, the shadowing fact, and the turn's invocation count against the cap*; AC-10 pins the count. Add the fields to `SkillInvoked` on TASK-210's additive pattern (`#[serde(default, skip_serializing_if = …)]`, with the four-leg skew test including its non-vacuity leg) and publish them from the site that already mints the event. **`SkillInvoked` still never carries the body** — that pin at `skill_turn.rs:~1868` is not negotiable. The client rendering is a TASK-219 follow-up, so land the fields before that resumes.
- [ ] **AC-13 has no owner, and BR-12's echo is unpublished for a model invocation.** TASK-216 found it: `SkillTool` holds no `EventBus` and no `SessionId`, and no task's file list gave it one, so a model-issued invocation raises no `SkillInvoked` at all — the session prints nothing and `/verbose` has nothing to add to. The user path's publish site in `runtime.rs` still carries the literal `InvokedBy::User`, which is correct for the path that reaches it. You own `build_tools`; give the tool what it needs to publish, and assert a model invocation raises the event with `InvokedBy::Model`. This is the same field the wire-fields AC above extends — do both in one pass.
- [ ] **Wire `TurnState::note_foreign_tool_completed()`, or BR-6b is wrong in one direction.** TASK-216 shipped it as an unwired seam: the tool cannot see the loop's other dispatches, so `skill alpha` → `read` → `skill alpha` in one turn is refused `repeated` where BR-6b admits it. BR-6b's *stated* example (`/proceed`'s two `/validate` passes separated by `/architect`) is admitted either way, because the intervening expansion overwrites the seed — which is exactly why a test written from the spec's example would pass with the seam unwired. Call it from `turn_loop`, and pin the case the example does not cover. (TASK-218 also touches `turn_loop`; coordinate or take it there.)
- [ ] Mutation: dropping `invoker` from `build_tools` fails the addressee test — and only that test, which is the point.

## Technical Notes

- Precedent, exactly: `WebTool` holds `gate: Arc<PermissionGate>` and `runtime: Handle` and bridges sync→async in `run` with `block_in_place`. `SkillTool` does the same with one more field.
- `skill_turn.rs`'s source-scans slice `run_prompt_turn` and `settle_dynamic_context` **by signature and by their terminating doc comments**. This task changes one of those signatures, so those scans will break for a reason unrelated to behaviour — widen them deliberately.

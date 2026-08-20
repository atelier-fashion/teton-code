---
id: TASK-204
title: "run_prompt_turn: expand, refuse before consent, seed with provenance — in that order"
status: draft
parent: REQ-585
created: 2026-08-20
updated: 2026-08-20
dependencies: [TASK-200, TASK-202, TASK-203]
---

## Description

The ordering BR-8 is really about. `CarriedTurn::begin` (`runtime.rs:2935`)
both pushes the user block **and** arms the drop-commit, so every check BR-8
requires has to happen before that line — not somewhere inside the turn loop.

This task lands the non-consent half: accept the invocation, expand it, refuse
it if the body alone does not fit, and seed the turn with the skill file's
provenance. TASK-205 adds the consent and the commands.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `run_prompt_turn`'s skill path
- `crates/tetond/src/server.rs` — `spawn_prompt_turn` / `flatten_prompt` carry the invocation
- `crates/tetond/tests/skill_turn.rs` — the ordering and refusal suite

**Inherited from TASK-203, which could not reach them from `server.rs`:**

### Inherited seams (both need `DaemonRuntime`, which `server.rs` cannot reach)

- [ ] **`drop_project_skill_grants` is called on `/cd`.** TASK-201 landed it in `crates/tetond/src/harness/permissions.rs`; nothing calls it. The session's `PermissionGate` lives behind a private `session_gates` and `permission_gate_for` needs a `&Config`, so the call site is inside `DaemonRuntime::set_session_cwd`. A grant remembered under `skill:project:<name>` in one repo must not authorize another repo's commands after the root moves — the registry is rebuilt and the name now means a different file (ADR-6, LESSON-501).
- [ ] **The skills rebuild moves inside `DaemonRuntime::set_session_cwd`, ahead of the `session_root_changed` publish.** It currently runs in `server.rs` *after* the runtime returns, so a second attached client that reacts to that event within microseconds reads the pre-move registry. Same-connection ordering is already safe (the reader loop is serial); this closes the cross-client window. `rebuild_session_skills`'s doc in `server.rs` carries the pointer.

## Acceptance Criteria

- [ ] `PromptTurnParams` validation: both populated ⇒ `INVALID_PARAMS` (a combination that was never valid, so nothing is narrowed). A both-empty request is **not** newly rejected — `flatten_prompt(&[])` returns `""` and such a turn runs today, and rejecting it would narrow an existing method for third-party clients while `PROTOCOL_VERSION` is asserted unchanged. The raw-`/name args`-reaching-a-model failure is already impossible: the CLI never puts the typed line in `prompt` (ADR-3).
- [ ] An unknown or shadowed skill name arriving from a client is refused by the daemon too — the client's snapshot is a convenience, not the authority (LESSON-520's shape: do not let the only check live on the far side of the wire).
- [ ] Order inside the turn: probe root → `expand` → route + `route.budget`, **over the expansion** → **Stage A refusal** → (TASK-205's consent and commands) → **Stage B refusal** → `CarriedTurn::begin`. Stage A measures the expansion with a `[dynamic context pending]` placeholder in each slot, so a body that cannot fit is refused **before** the user is asked to approve anything (BR-8d).
- [ ] **Expansion precedes routing, and it is the point of this task.** `dispatch_route(..., &prompt)` (`runtime.rs:2830`) runs the freeform classifier over the prompt text, and `spawn_title_session(..., &prompt)` (`:2858`) spends the session's one naming attempt on it — both ~100 lines before the harness is assembled. A skill turn's `prompt` is empty (ADR-3), so expanding after routing classifies and names every invocation from `""`. On a machine with per-category bindings that can route `/analyze` to the local tier and then refuse it by its own budget check — AC-20(c) failing — and it leaves the session named from nothing for its whole life. Two assertions: the classifier receives the expansion, and the title attempt receives it.
- [ ] Routing sees the **body-only** expansion, before dynamic output is folded in — the classifier reads the skill's instructions, and the alternative would make the route depend on output that the route's own permission level decides whether to produce. Stated in a comment at the call site, not only here.
- [ ] A refused turn emits **no** `context_pressure` event of any kind and no newest-user elision note. Asserted as a drain-and-assert-empty, copying `context_pressure.rs:786 a_report_with_nothing_in_it_is_the_one_that_says_nothing` and `runtime.rs:27432` (BR-8c).
- [ ] A refused turn changes no health, degrades nothing, and does not retry — the four properties of REQ-586's sibling arm, asserted the same way (`runtime.rs:27362`).
- [ ] A typed oversized prompt **still elides** loudly (REQ-586 BR-7), pinned in the same file so the refusal is seen to apply to skill turns only (AC-16).
- [ ] The turn is seeded with `push_user_from(text, sources)` where sources is the skill file's id — or the unpinnable marker for a user skill outside the root (ADR-9, TASK-197).
- [ ] The `digest` duty never touches the expansion: `summarize_if_large`'s only production call site stays the tool-result fold. Pinned, because REQ-586 scaled the digest thresholds with the route budget and a skill body is squarely inside the band that would trigger it (BR-4).
- [ ] AC-13's teeth: a skill declaring `model: opus`, `effort: max` and `allowed-tools: Bash(*)` produces **exactly** the route, effort and permission level a typed prompt would. This is BR-5's guarantee and OQ-5's resolution — a file on disk must not escalate spend — and it needs a harness that can see a route, which TASK-195's pure suite cannot (LESSON-481). It lives here.
- [ ] The prompt handed to the engine is asserted from a test that drives `run_prompt_turn` itself, not from a hand-seeded `CarriedTurn::begin` fixture. Hand-building the expansion and asserting it arrived leaves the producer unguarded, and a daemon that stopped substituting `$ARGUMENTS` would keep the test green (LESSON-544).
- [ ] Mutation table: moving either refusal after `CarriedTurn::begin`, moving Stage A after the consent, and routing before expanding each fail a named test.

## Technical Notes

- `flatten_prompt` (`server.rs:1974`) runs before the spawn and returns a `String`. The invocation must travel beside it, not through it — a skill turn has no `PromptBlock`s to flatten.
- The expansion is built earlier than `runtime.rs:2910` — it needs only the registry and the raw argument string, both available before `dispatch_route`. Everything *else* this task needs is in hand at `:2910-2935`: `probed`, `tool_ctx`, `route.harness`, `route.budget`, `system`. Do not re-probe and do not re-derive the budget.

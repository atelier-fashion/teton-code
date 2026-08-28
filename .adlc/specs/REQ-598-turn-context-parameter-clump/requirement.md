---
id: REQ-598
title: "TurnContext: dissolve the parameter clump the suppressions have been hiding"
status: draft
deployable: true
created: 2026-08-28
updated: 2026-08-28
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "extensibility", "reliability"]
tags: ["refactor", "parameter-clump", "too-many-arguments", "turn-context", "traceability"]
---

## Description

The workspace carries **25** `#[allow(clippy::too_many_arguments)]`
suppressions — 14 in `runtime.rs` alone, 4 in `harness/turn_loop.rs`, the rest
scattered across `carry.rs`, `budget.rs`, `engine.rs`, `category.rs`,
`tools/skill.rs`, and `main.rs`. They are not 25 independent design choices.
Nearly all of them carry the same recurring cluster: `events: &Arc<EventBus>`,
`session_id: &SessionId`, `config: &Config`, `router: &Router`, and
`gate: &PermissionGate`. `offer_or_refuse_over_budget` takes 12 parameters;
`build_duty_route` and `resolve_duty` each carry the attribute **twice**, stacked
— a copy-paste artifact that is itself evidence nobody is reading these lines
any more.

A suppression is a lint told to stop reporting a fact. The fact is still true:
this cluster is an unnamed concept being passed by hand through every layer of
the turn path. Naming it as `TurnContext` removes most of the suppressions as a
side effect, but the actual value is that adding a new per-turn fact stops
requiring an edit to a dozen signatures and their call sites.

This REQ is a **behavior-preserving refactor**. Its risk is not that it breaks a
test; it is that it silently relocates a call whose *ordering* is load-bearing.
The retrieved lessons below name four such orderings by hand, and the acceptance
criteria pin each one.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `TurnContext` | `events` | `Arc<EventBus>` | Present for every turn |
| `TurnContext` | `session_id` | `SessionId` | Present for every turn |
| `TurnContext` | `config` | snapshot or handle | **Read after the turn claim**, never before (BR-4) |
| `TurnContext` | `router` | `&Router` / handle | Present for every turn |
| `TurnContext` | `gate` | `Arc<PermissionGate>` | Present for every turn |
| `TurnContext` | `route` | `Route` | Set once routing has resolved; absent before |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| _None._ | This REQ introduces no new events and must change no existing event's payload or emission order. | |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| _Unchanged._ | The permission model is untouched by this REQ. |

## Business Rules

- [ ] BR-1: The refactor is **behavior-preserving**. No event payload, event ordering, error code, refusal sentence, or dispatch decision changes. A user cannot tell this REQ shipped.
- [ ] BR-2: `TurnContext` construction happens **after** the turn is claimed, and reads session state (`cwd`, `session_root`) from the registry at that point. It must not accept a snapshot taken in `spawn_prompt_turn` before the claim (informed by LESSON-539, REQ-583 — the pre-claim snapshot is a TOCTOU race that was already fixed once).
- [ ] BR-3: Request-id minting for daemon-wide resources stays centralized in `PendingPermissions`. `TurnContext` must not acquire a per-session counter for a daemon-wide namespace (informed by BUG-161 — that collision cross-authorized tool calls across sessions).
- [ ] BR-4: Any filesystem I/O reachable from `TurnContext` construction stays off the connection reader loop, via the existing `block_in_place_if_multithread` seam (informed by BUG-184 — synchronous skill discovery on the reader loop stalled RPCs behind a TCC dialog).
- [ ] BR-5: Security gates that currently run **before** deserialization continue to run before it. `TurnContext` construction must not be inserted between a gate and the parse it guards (informed by LESSON-520 — a gate that moves after the parse makes its own refusal test vacuous).
- [ ] BR-6: Injectable test seams survive. `TurnContext` is constructible with test doubles (`AlwaysFailsVerifier`, counting gates) and honors the `TETON_PRESENCE_ACCEPT=fail` env seam (informed by LESSON-519).
- [ ] BR-7: The distinction between cap-exempt (mandatory) and optional tools is not absorbed into `TurnContext` in a way that hides it. Ordering-dependent registry logic stays visible (informed by LESSON-496 — "cut first under pressure" became "never available" when a limit equalled the mandatory count).
- [ ] BR-8: Every REQ / ADR / LESSON / BUG reference in a moved doc comment moves **with** the code it annotates. A relocated function that loses its `REQ-588 BR-3 / ADR-4` header is a defect under this REQ, not an acceptable cost.
- [ ] BR-9: The net count of `#[allow(clippy::too_many_arguments)]` in the workspace strictly decreases, and no new suppression of any kind is introduced to make the refactor compile.

## Acceptance Criteria

- [ ] AC-1: The workspace `#[allow(clippy::too_many_arguments)]` count drops from 25 to a number recorded verbatim in the PR body, and a test asserts the count does not regress above that number.
- [ ] AC-2: The doubled `#[allow(clippy::too_many_arguments)]` on `resolve_duty` and `build_duty_route` is gone (both sites, not one).
- [ ] AC-3: `cargo clippy --workspace --all-targets` stays clean under the workspace's `clippy::all = deny`.
- [ ] AC-4: `cargo test --workspace --no-fail-fast` is green, and the output is grepped for `FAILED` rather than trusting a summed pass count (conventions.md — an interrupted fail-fast run reports a floor, not a total).
- [ ] AC-5: A test mutates `cwd` between turn spawn and turn claim and asserts `TurnContext` observes the **fresh** value — this is the BR-2 / LESSON-539 regression guard, and it must fail if `TurnContext` is constructed from a pre-claim snapshot.
- [ ] AC-6: The existing `ParkingVerifier` reader-loop test still proves concurrent RPCs are served while a presence gate blocks (BR-4 guard; informed by LESSON-518 — routing tests alone cannot show this).
- [ ] AC-7: The gate-before-parse refusal tests still use **valid, persistable** payloads paired with an acceptance case, so they remain non-vacuous after the move (BR-5 guard; informed by LESSON-520).
- [ ] AC-8: **Traceability sweep** — a check enumerates REQ/ADR/LESSON/BUG ids present in the touched files before and after, and fails on any id that disappeared. This is a **region check**, not a count: a comment relocated to the wrong function keeps the count identical (informed by conventions.md / LESSON-568).
- [ ] AC-9: The three typed-outcome turn-path arms (`PrivacyBlocked`, `ContextLengthExceeded`, `SpendCeilingReached`) each still have both halves — `failure_class() -> None` **and** a dedicated arm ordered before the generic remote arm. A test inverts each arm and confirms the user-facing sentence changes (informed by conventions.md's both-halves rule and LESSON-557).
- [ ] AC-10: Event ordering is asserted unchanged for at least one full turn, by comparing a recorded event sequence against a pre-refactor fixture (BR-1 guard).

## External Dependencies

- None. This is an internal refactor.

## Assumptions

- The five-field cluster is genuinely the same concept at every site. If a subset of the 25 sites turns out to want a *different* bundle, the answer is two small structs, not one wide one — and that discovery belongs in `/architect`, not here.
- `Config` can be carried by handle or cheap snapshot without changing read-after-claim semantics (BR-2). If it cannot, BR-2 wins over convenience.
- Behavior preservation is checkable by the existing ~3,650-test suite plus the guards above. The suite passing is necessary, not sufficient — REQ-585 and REQ-587 each shipped three Criticals past a green suite.

## Open Questions

- [ ] OQ-1: Does `TurnContext` own `route`, or is a routed turn a distinct type (`RoutedTurn`) so that "route not yet resolved" is unrepresentable rather than an `Option`? The typestate version is safer and a larger diff.
- [ ] OQ-2: Should the struct be per-turn or per-session with a per-turn view? Per-session risks BR-2's staleness class returning by another door.
- [ ] OQ-3: Do the non-`runtime.rs` sites (`engine.rs`, `category.rs`) share this cluster, or do they carry a different one that merely also trips the lint? If the latter, they are out of scope and their suppressions stay — and AC-1's target number must reflect that.

## Out of Scope

- Splitting `runtime.rs` into multiple modules — that is REQ-599, which this REQ makes tractable.
- Reducing the length of `run_prompt_turn` itself (REQ-599).
- Any behavior change, including "obvious" fixes noticed while moving code. Those get filed, not folded in.

## Retrieved Context

- LESSON-570 (lesson, score 9): A prompt sentence must be true after the REQ ships, not before it
- REQ-589 (spec, score 9): Offer to proceed when a skill expansion exceeds the route's context budget
- REQ-591 (spec, score 9): The project-skill trust gate and its unattended allowlist
- LESSON-518 (lesson, score 9): A blocking gate's reader-loop freedom is not inherited from the await path
- LESSON-519 (lesson, score 9): An "assert by inspection, not from the error" AC needs the real artifact
- LESSON-520 (lesson, score 9): A gate that fires before deserialization makes an invalid-payload test vacuous
- REQ-572 (spec, score 9): Capability-aware refusals and guided in-session enablement
- REQ-567 (spec, score 9): Cross-prompt conversation carry in interactive sessions
- REQ-587 (spec, score 8): Model-invoked skills
- LESSON-496 (lesson, score 8): "Cut first under pressure" means "never available" when the limit equals the floor
- BUG-184 (bug, score 7): Skill discovery runs on the connection's synchronous reader loop
- LESSON-539 (lesson, score 7): Claim first, then re-read — session state snapshotted before the turn claim is stale
- REQ-575 (spec, score 7): Presence attestation for the web setup commit
- REQ-576 (spec, score 7): Presence attestation for config/set
- BUG-161 (bug, score 7): Permission request_ids collide across concurrent sessions

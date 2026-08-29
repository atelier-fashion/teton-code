---
id: REQ-598
title: "TurnContext: dissolve the parameter clump the suppressions have been hiding"
status: approved
deployable: true
created: 2026-08-28
updated: 2026-08-29
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
| ~~`TurnContext`~~ | ~~`route`~~ | ~~`Route`~~ | **Removed in Phase 2 (ADR-3, answering OQ-1).** `route` is reassigned on every fallback reroute inside `run_one_attempt`'s `'turn:` loop, so a context owning it would go stale or need rebuilding each iteration. It stays an explicit parameter — which also keeps the reroute visible in the signature, per BR-7. |
| `DutyContext` | `local_engine` | `Option<&(Arc<Mutex<dyn Engine>>, ChatFormat)>` | Added in Phase 2 (ADR-1); travels with every duty resolution |
| `DutyContext` | `prompt_spend` | `Option<&Arc<PromptSpend>>` | Added in Phase 2 (ADR-1); travels with every duty resolution |

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
- [ ] BR-2.1 (added in Phase 2 — ADR-4): BR-2 names **one instance of a class**.
  The class is: *a context must not be constructed before any point that rebinds
  a field it captures.* The turn claim is one such point. **The REQ-580 warming
  hold is another, and BR-2 does not cover it**: when the local tier is warming,
  `run_prompt_turn` shadow-rebinds `router` after the hold wakes, building a
  fresh one from the settled tier state and re-dispatching the route. A
  `TurnContext` constructed after the claim but *before* the hold satisfies BR-2
  and still carries a stale `router` to every downstream consumer — silently
  breaking REQ-580's guarantee that "a turn served after the wait must be built
  from the route it is served *by*." Construction happens after the **last**
  rebinding of every captured field, and a test asserts the captured `router` is
  the post-hold one on a warming-tier turn (informed by LESSON-586 — a rule that
  names one field of a validated class is a rule about the class).
- [ ] BR-3: Request-id minting for daemon-wide resources stays centralized in `PendingPermissions`. `TurnContext` must not acquire a per-session counter for a daemon-wide namespace (informed by BUG-161 — that collision cross-authorized tool calls across sessions).
- [ ] BR-4: Any filesystem I/O reachable from `TurnContext` construction stays off the connection reader loop, via the existing `block_in_place_if_multithread` seam (informed by BUG-184 — synchronous skill discovery on the reader loop stalled RPCs behind a TCC dialog).
- [ ] BR-5: Security gates that currently run **before** deserialization continue to run before it. `TurnContext` construction must not be inserted between a gate and the parse it guards (informed by LESSON-520 — a gate that moves after the parse makes its own refusal test vacuous).
- [ ] BR-6: Injectable test seams survive. `TurnContext` is constructible with test doubles (`AlwaysFailsVerifier`, counting gates) and honors the `TETON_PRESENCE_ACCEPT=fail` env seam (informed by LESSON-519).
- [ ] BR-7: The distinction between cap-exempt (mandatory) and optional tools is not absorbed into `TurnContext` in a way that hides it. Ordering-dependent registry logic stays visible (informed by LESSON-496 — "cut first under pressure" became "never available" when a limit equalled the mandatory count).
- [ ] BR-8: Every REQ / ADR / LESSON / BUG reference in a moved doc comment moves **with** the code it annotates. A relocated function that loses its `REQ-588 BR-3 / ADR-4` header is a defect under this REQ, not an acceptable cost.
- [ ] BR-9: The net count of `#[allow(clippy::too_many_arguments)]` in the workspace strictly decreases, and no new suppression of any kind is introduced to make the refactor compile.

## Acceptance Criteria

- [ ] AC-1: The workspace `#[allow(clippy::too_many_arguments)]` count drops from
  25 to a number recorded verbatim in the PR body, and a test asserts the count
  does not regress above that number. The PR body MUST report the drop **split
  into its two disjoint populations**, because a single number credits the
  refactor with removals that required no refactor:
  **(a) vestigial** — attributes that suppress nothing, removable with no
  signature change; **(b) earned** — attributes on functions that genuinely
  tripped the lint and stopped tripping it because the cluster became a
  parameter. Phase 1 measured the baseline by stripping all 25 and re-running
  clippy with the lint downgraded to a warning (under `all = deny` the build
  aborts at the first crate and reports only one site): **16 of 25 fire, 9 are
  vestigial** — the 2 AC-2 duplicates, the 5 `*_route` functions (exactly 7
  arguments each, sitting *at* clippy's default threshold), and the 2 in
  `engine.rs`. The count test greps the **source tree**, not clippy output, so
  it also covers the feature-gated sites AC-3 cannot reach (see AC-3).
- [ ] AC-2: Neither `resolve_duty` nor `build_duty_route` retains a **stacked
  pair** of `#[allow(clippy::too_many_arguments)]` — the fix is applied to both
  *functions*, not one. Because both collapse below the threshold once the duty
  cluster becomes a parameter, each must end with **zero** such attributes, not
  one; a lone surviving attribute on either is a vestigial suppression under
  AC-1(a) and fails this criterion.
- [ ] AC-3: `cargo clippy --workspace --all-targets` stays clean under the
  workspace's `clippy::all = deny`. **Known coverage limit, recorded rather than
  papered over**: this command does not compile `teton-inference`'s
  `#[cfg(feature = "llama")]` block, so it never checks `engine.rs::serve` or
  `engine.rs::run_generation`. Those two sites are out of scope per OQ-3 and are
  covered only by AC-1's source-tree count. Do not read a green AC-3 as a
  workspace-wide statement.
- [ ] AC-4: `cargo test --workspace --no-fail-fast` is green, and the output is grepped for `FAILED` rather than trusting a summed pass count (conventions.md — an interrupted fail-fast run reports a floor, not a total).
- [ ] AC-5: A test mutates `cwd` between turn spawn and turn claim and asserts
  `TurnContext` observes the **fresh** value — the BR-2 / LESSON-539 regression
  guard. **The existing test does not satisfy this criterion and reusing it is a
  defect** (Phase 1 finding, LESSON-586's shape):
  `a_turn_handed_a_stale_cwd_snapshot_runs_on_the_root_the_registry_holds_at_claim_time`
  (`runtime.rs`) guards `run_prompt_turn`'s post-claim re-read of `session_cwd`,
  which sits **upstream** of every point at which a `TurnContext` can be built.
  A context constructed anywhere below that re-read inherits the fresh value for
  free, so that test cannot distinguish a correctly-built `TurnContext` from one
  built off the pre-claim snapshot — a broader guard standing in front of the
  mutation this AC exists to catch.
  Satisfying AC-5 therefore requires **all three**:
  (a) the guard's subject is `TurnContext`'s **own** view of the root (assert on
  what the constructed context carries, not only on the turn's downstream
  behavior), so the assertion is not satisfiable by the upstream re-read alone;
  (b) the mutation is **demonstrated**: build `TurnContext` from the
  `session_cwd` *parameter* (the pre-claim snapshot) instead of the post-claim
  re-read, confirm the test goes **red**, and revert;
  (c) that mutation and its observed failure are recorded verbatim in the test's
  doc comment, per conventions.md ("show the test can fail before trusting that
  it passed").
- [ ] AC-6: The existing `ParkingVerifier` reader-loop test still proves concurrent RPCs are served while a presence gate blocks (BR-4 guard; informed by LESSON-518 — routing tests alone cannot show this).
- [ ] AC-7: The gate-before-parse refusal tests still use **valid, persistable** payloads paired with an acceptance case, so they remain non-vacuous after the move (BR-5 guard; informed by LESSON-520).
- [ ] AC-8: **Traceability sweep — a true region check.** A per-file set diff of
  REQ/ADR/LESSON/BUG ids is **not sufficient and does not satisfy this
  criterion** (Phase 1 finding). Evidence: when REQ-597 rebased onto REQ-596,
  a method was inserted between `config_snapshot`'s doc comment and its
  attribute, orphaning the comment from the item it documents. No id left the
  file — set identical, count identical, defect present. The check MUST bind
  each id to the **item it annotates**, and fail on all three of:
  (a) **Disappearance** — an id present in a touched file before the refactor is
  absent after, anywhere in the workspace (an id may legitimately *move between
  files*; the sweep is workspace-scoped so a genuine relocation is not a false
  positive).
  (b) **Re-association** — an id whose owning item changed. For every id, record
  the set of item names (fn/struct/impl) whose attached doc-comment block
  carries it; fail if that mapping changes, except where the PR body names the
  rename explicitly.
  (c) **Orphaning** — a doc-comment block that is no longer attached to any
  item: a `///` run, or a `//` run immediately preceding an item, separated from
  its item by a blank line or by an intervening item. This arm is what catches
  the REQ-596/597 failure above, and it MUST be shown to catch it — reproduce
  that exact insertion against `config_snapshot`, confirm the sweep goes red,
  and revert (conventions.md's invert-the-gate rule).
- [ ] AC-9: The three typed-outcome turn-path arms (`PrivacyBlocked`, `ContextLengthExceeded`, `SpendCeilingReached`) each still have both halves — `failure_class() -> None` **and** a dedicated arm ordered before the generic remote arm. A test inverts each arm and confirms the user-facing sentence changes (informed by conventions.md's both-halves rule and LESSON-557).
- [ ] AC-10: Event ordering is asserted unchanged for at least one full turn, by
  comparing a recorded event sequence against a pre-refactor fixture (BR-1
  guard). **Fixture provenance is load-bearing and pinned**: the golden sequence
  is captured on the **base commit** (`origin/main`, before any refactor commit
  lands on the branch) and committed as a checked-in file in the **first**
  task's commit. A fixture regenerated after the refactor is an oracle computed
  by the subject — conventions.md's first named trap — and does not satisfy this
  criterion. The test must additionally be shown to fail when two events are
  transposed; record that mutation in its doc comment.

## External Dependencies

- None. This is an internal refactor.

## Assumptions

- The five-field cluster is genuinely the same concept at every site. If a subset of the 25 sites turns out to want a *different* bundle, the answer is two small structs, not one wide one — and that discovery belongs in `/architect`, not here.
- `Config` can be carried by handle or cheap snapshot without changing read-after-claim semantics (BR-2). If it cannot, BR-2 wins over convenience.
- Behavior preservation is checkable by the existing ~3,650-test suite plus the guards above. The suite passing is necessary, not sufficient — REQ-585 and REQ-587 each shipped three Criticals past a green suite.

## Open Questions

- [ ] OQ-1: Does `TurnContext` own `route`, or is a routed turn a distinct type (`RoutedTurn`) so that "route not yet resolved" is unrepresentable rather than an `Option`? The typestate version is safer and a larger diff.
- [ ] OQ-2: Should the struct be per-turn or per-session with a per-turn view? Per-session risks BR-2's staleness class returning by another door.
- [x] OQ-3: **ANSWERED in Phase 1 — no, they do not share the cluster.** Measured
  by reading all 25 signatures. Zero of the five cluster fields (`events`,
  `session_id`, `config`, `router`, `gate`) appear in any of them:
  `engine.rs::serve` takes `backend, model, n_ctx, resident, cache, cache_key,
  prompt, params, out_tx, ctrl_rx`; `engine.rs::run_generation` takes `ctx,
  model, tokens, start, params, out_tx, ctrl_rx`; `category.rs::resolve_fallback`
  takes `category, tier, primary, rejected, source, fallback, health, usable`.
  `category.rs` lives in `teton-core`, which by architecture performs no I/O and
  therefore has no access to `EventBus` or `PermissionGate` at all — it *cannot*
  take this cluster. Both `engine.rs` sites sit inside the
  `#[cfg(feature = "llama")]` block and are compiled by neither AC-3's command
  nor CI. **All three are out of scope; their suppressions stay.** Two further
  sites fail the same test and are likewise out of scope:
  `main.rs::provider_add_on` (CLI plumbing — `conn`, `ctx`, `keychain`) and
  `budget.rs::skill_append_fit` (pure fit arithmetic).
  `skill.rs::publish_invocation` is **precedent, not target**: its doc comment
  records that it already reads session and bus *off the gate* rather than
  taking them as parameters — the bundling this REQ generalizes.
- [ ] OQ-4 (raised in Phase 1): `runtime.rs` holds **two** clusters, not one, and
  `/architect` must decide whether that means two structs. The turn cluster
  (`events, session_id, config, router, gate`, + `route`) appears in
  `offer_or_refuse_over_budget`, `build_tools`, `run_one_attempt`. The
  duty-routing cluster (`router, config, events, session_id` — **no gate** —
  plus `local_engine` and `prompt_spend`, which travel with it at every site)
  appears in `resolve_duty`, `build_duty_route`, `spawn_title_session`, and the
  five `*_route` functions. `turn_loop.rs` carries a third, harness-layer
  cluster (`tools, tool_ctx, gate, events: &SessionEvents, ctx, config:
  &HarnessConfig, hook`) with no `session_id` and no `router`. The spec's own
  Assumptions section already rules on the principle — "two small structs, not
  one wide one" — and this is the discovery it anticipated.

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

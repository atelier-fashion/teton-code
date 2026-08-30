---
id: REQ-599
title: "Decompose the turn path and split runtime.rs, without losing the traceability that makes it readable"
status: approved
deployable: true
created: 2026-08-28
updated: 2026-08-29
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability", "extensibility"]
tags: ["refactor", "god-module", "decomposition", "run-prompt-turn", "traceability", "runtime"]
---

## Description

`crates/tetond/src/runtime.rs` is a god module. **Measured on `main` at
`fedcab1`, 2026-08-29** — a snapshot that drifts by tens of lines a day, which
is why AC-1 records its target in the architecture doc rather than hard-coding
one here:

| | |
|---|---|
| `runtime.rs` total | 36,747 |
| …of which **production** | **14,183** |
| …in-file `#[cfg(test)]` | 22,564 (61%) |
| `pub fn` in production | 67 |
| `run_prompt_turn` | **1,084 lines** |
| `turn_loop.rs::run_session_turn_with_pressure_policy` | **762 lines**, 8–9 levels of nesting |

Even discounting tests correctly, the production half is roughly 47× the
~300-line splitting threshold, spanning session lifecycle, routing, skill
dispatch, model consent, provider setup and migration, cost-ledger wiring, and
MCP egress. `run_prompt_turn` alone carries ~109 branch points — session
claiming, skill expansion, routing, budget checks, consent, dispatch and commit
in one `async fn`.

The original draft cited 36,085 / 14,126 / 1,088 / 64, measured 2026-08-28.
REQ-598 has since landed; the figures moved, the shape of the problem did not.

The honest counter-argument, and it is a real one: this file is *heavily and
deliberately documented*. Its comments carry REQ, ADR, LESSON, and BUG ids that
explain why each branch is ordered the way it is — the `SpendCeilingReached` arm
literally explains why it must precede the generic remote arm. That density is
an asset, and a mechanical split is the fastest way to destroy it. Several
subsystems were kept colocated with their tests on purpose.

So this REQ is not "split the file." It is: **decompose along the seams the
documentation already names, and make the traceability survivable as a checked
property rather than a hope.** If a proposed boundary cannot be drawn without
orphaning a rationale comment, that is evidence the boundary is wrong.

This REQ depends on REQ-598, which names the parameter cluster that currently
makes any extraction produce 10-argument functions.

## System Model

### Entities

| Entity | Field | Type | Constraints |
|--------|-------|------|-------------|
| `TurnStage` | `name` | enum | The named phases of a turn: claim, expand, route, budget, consent, dispatch, commit |
| `TurnStage` | `rationale_ids` | list of string | REQ/ADR/LESSON/BUG ids owned by that stage; the traceability unit |
| `ModuleBoundary` | `path` | string | A destination module extracted from `runtime.rs` |
| `ModuleBoundary` | `owned_ids` | list of string | Ids that must appear in that module after the move |

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| _None._ | No new events; no existing event's payload or ordering changes. | |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| _Unchanged._ | | 

## Business Rules

- [ ] BR-1: Behavior-preserving. No event payload, ordering, error code, refusal sentence, or dispatch decision changes. This includes the *text* of user-facing sentences.
- [ ] BR-2: Every REQ / ADR / LESSON / BUG id present in `runtime.rs` and `turn_loop.rs` before the split is present after it, **in the module that owns the code it explains**. An id that survives in the wrong module is a failure, not a pass (informed by conventions.md / LESSON-568 — a count-based sweep cannot see a relocation).
- [ ] BR-3: The turn path's **ordering invariants** are preserved and made explicit. At minimum: the three typed-outcome arms stay ordered before the generic remote arm; gates stay before the parses they guard (LESSON-520); the claim stays before the session-state read (LESSON-539); presence gates keep their reader-loop freedom (LESSON-518, BUG-184).
- [ ] BR-4: `run_prompt_turn`'s decomposition preserves the **accumulators** whose lifetime spans stages — the prompt spend accumulator consulted by the spend ceiling, the context-manager block set subject to withdrawal on `context_length_exceeded`, and the session-naming duty which must not be spent on a not-sent path (informed by REQ-589 BR-11, BR-12, BR-14.1).
- [ ] BR-5: Conversation-carry invariants hold across the split: turn blocks join `SessionConversation` **atomically** on completion, a failed turn leaves conversation state exactly as at turn start, and out-of-band duty output never joins the conversation (informed by REQ-567 BR-6).
- [ ] BR-6: Assembled context stays byte-identical regardless of KV-cache state; no extracted stage may make context assembly depend on cache warmth (informed by REQ-567 BR-7).
- [ ] BR-7: Tests move with the code they exercise. A subsystem extracted to a new module takes its `#[cfg(test)]` bodies with it; tests are not left behind pointing at a module they no longer describe.
- [ ] BR-8: No new `#[allow(...)]` of any kind is introduced to make the split compile.
- [ ] BR-9: The split is delivered as a **sequence of independently reviewable commits**, one boundary at a time, each green. A single 14,000-line move commit is not acceptable — it is unreviewable, which is the condition this REQ exists to end.

## Acceptance Criteria

- [ ] AC-1: `runtime.rs` production line count drops below a target recorded in the architecture doc, and no extracted module exceeds it either — the file is not merely split into two god modules.
- [ ] ~~AC-2: `run_prompt_turn` is reduced to a stage sequence whose body is under 200 lines.~~ **Deferred out of scope — see Out of Scope.** This is architecture.md's step 8, the only step that restructures control flow rather than relocating code. Split to its own REQ so it can be reviewed as a change rather than buried inside a relocation diff.
- [ ] ~~AC-3: `run_session_turn_with_pressure_policy`'s maximum nesting depth drops to 5 or below.~~ **Deferred** — `turn_loop.rs` was already OQ-3's open question, and it sits in a different module with a different parameter cluster (REQ-598 ADR-2). It rides with the deferred step 8.
- [ ] AC-4: **Traceability check** — an automated check extracts every REQ/ADR/LESSON/BUG id from the pre-split files and asserts each appears post-split in its declared owning module. It fails on a disappeared id **and** on an id that moved to an unexpected module (BR-2). This check ships with the REQ and stays in CI.
- [ ] AC-5: **Mutation test on AC-4** — deleting one rationale comment causes the check to fail; relocating one to the wrong module also causes it to fail. Both mutations are recorded in the check's doc comment. Without this, AC-4 is exactly the "count that cannot see a relocation" LESSON-568 warns about.
- [ ] AC-6: An event-sequence fixture recorded before the split replays identically after it, for a turn that exercises: skill expansion, a routing decision, a consent prompt, and a successful dispatch (BR-1).
- [ ] AC-7: A turn that fails mid-dispatch leaves `SessionConversation` byte-identical to its pre-turn state (BR-5), asserted by comparing serialized state, not by checking an error code.
- [ ] AC-8: An over-budget turn that the user declines emits no `context_pressure`, does not spend the session-naming duty, does not degrade provider health, and does not dispatch — asserted individually, per REQ-589 BR-11 (BR-4 guard).
- [ ] AC-9: A turn accepted over budget and then failing with `context_length_exceeded` withdraws the expansion block and absorbs its provenance into `DroppedProvenance` (REQ-589 BR-14.1).
- [ ] AC-10: `cargo test --workspace --no-fail-fast` green, output grepped for `FAILED`; `cargo clippy --workspace --all-targets` clean; `cargo fmt --check` clean.
- [ ] AC-11: Each commit in the sequence is independently green in CI (BR-9), demonstrated by the PR's commit-status history.
- [ ] AC-12: `.adlc/context/architecture.md` is updated to describe the post-split module map, and a test asserts every module named in that doc exists on disk — so the map cannot silently rot.

## External Dependencies

- **REQ-598** (`TurnContext`) should land first. Extracting stages before the parameter cluster is named produces functions with 10+ arguments, which is the current state relocated rather than improved.

## Assumptions

- ~~The documentation's REQ/ADR/LESSON ids are a usable proxy for the real seams.~~
  **REFUTED in Phase 2 — see architecture.md ADR-1.** Measured: of 19 REQ ids
  appearing on 3+ production items, **1 is clustered and 13 are scattered across
  the whole file** (REQ-561 spans 13,547 of 14,183 lines). Inside
  `run_prompt_turn`, only 4 of 13 ids are local to one stage while 5 span the
  entire function. A REQ id marks *a change*, and changes to a turn path are
  overwhelmingly cross-cutting; it records which decision a line serves, not
  which subsystem it belongs to. Read literally, the rule "where they interleave
  across a proposed boundary, the boundary is wrong" would condemn every
  possible boundary and make this REQ unimplementable — which is the tell that
  the rule was wrong, not the file. The ids remain an asset to **preserve**
  (BR-2, enforced by the sweep REQ-598 shipped) rather than a signal to
  **navigate by**. ADR-2 replaces the method: seams come from the type/`impl`
  structure, where the measurement is unambiguous — one `impl DaemonRuntime`
  block of ~7,143 lines is the god module, and duty routing is the one grouping
  already cohesive by position.
- The 61% test fraction in `runtime.rs` means most of the raw line count moves mechanically with BR-7, and the genuinely hard part is the 14,126 production lines.
- No in-flight REQ is concurrently rewriting `run_prompt_turn`. If one is, this REQ waits — a rebase across a 14k-line move is worse than a delay.

## Open Questions

- [ ] OQ-1: What is the right module map? Candidate seams from the current `pub fn` census: session lifecycle, routing/duty resolution, skill dispatch, model consent, provider setup/migration, cost-ledger wiring, MCP egress. This is `/architect`'s primary question and should produce ADRs.
- [ ] OQ-2: Should in-file `#[cfg(test)]` bodies move to `tests/` integration files during the split, or stay colocated? Colocation is the current deliberate style and BR-7 assumes it continues; changing it doubles the diff.
- [ ] OQ-3: Is `turn_loop.rs`'s 762-line function in scope here, or its own follow-up? It shares BR-3's ordering invariants but sits in a different module with a different owner.
- [ ] OQ-4: Does AC-1's target line number belong in the architecture doc (visible, reviewable) or in the CI check (enforced)? Both, probably — but then they can disagree.

## Out of Scope

- **`run_prompt_turn`'s restructure into a stage sequence (architecture.md step
  8), and `turn_loop.rs`'s nesting — deferred to their own REQ.** Steps 1–7
  *relocate* code; step 8 *changes control flow* on the path every prompt runs
  through. Landing them together would bury the one genuine behavior risk inside
  a diff dominated by relocation — and with tests at 61% of the file, that diff
  is several times the size of the change hiding in it. Deferring keeps every
  commit in this REQ reviewable as what it is: a move. The deferred work
  inherits AC-2, AC-3, and BR-3's ordering invariants, and is the natural
  follow-on once `runtime/turn.rs` is small enough to read.

- Any behavior change. Defects noticed during the move are filed as BUGs, not fixed in-flight (conventions.md; a behavior fix hidden inside a 14k-line move is unreviewable).
- Splitting `server.rs` (5,169 production lines), `session_ui.rs` (4,132), `main.rs` (4,210), `events.rs` (3,815), `permissions.rs` (3,521), or `budget.rs` (2,951) — re-measured at `fedcab1`. Each deserves the same treatment and its own REQ; doing them together reproduces the unreviewable-diff problem at workspace scale.
- Introducing new abstractions beyond what the extraction requires.

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

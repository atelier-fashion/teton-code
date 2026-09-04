---
id: TASK-386
title: "Runtime wiring: `GenerationState` on the record, the first-turn hook, short-circuits, and `Init`"
status: complete
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-04
dependencies: [TASK-379, TASK-384, TASK-385]
---

## Description

ADR-1 and ADR-2's short-circuits and ADR-6's outer function: the session record's
`GenerationState`, set at create and `/cd`; `assemble_harness` consuming `Pending` before the
turn's model call; the `never`/`plan`/`always` short-circuits before the gate; `session/context`
handling `Init { force }` as the explicit door. Covers BR-1, the `always`/`plan` halves of BR-2,
BR-8's daemon half, BR-10's daemon half.

## Files to Create/Modify

- `crates/tetond/src/sessions.rs` — `generation: GenerationState`, accessors.
- `crates/tetond/src/runtime/session.rs` — set the state in `store_session_repo_context` (create
  and `/cd`); `session_context` handles `Init`.
- `crates/tetond/src/runtime/turn.rs` — in `assemble_harness`, after REQ-612's refresh: if
  `Pending`, call `repo_context::generate::offer_and_run`; the block the loader produced is
  stamped as REQ-612 stamps it.
- `crates/tetond/src/repo_context/generate.rs` — `offer_and_run` (short-circuits → gate → `run`).
- `crates/tetond/src/server.rs` — nothing new if `session/context` already dispatches; assert.

## Acceptance Criteria

- [x] BR-1 / AC-1: first prompt with `absent` raises exactly one offer; accepted → written,
      loaded, and the same turn's request body ends with the block; declined → nothing, and a
      second prompt raises no offer; `/cd` to another absent project raises again; an
      `AGENTS.md` or empty `TETON.md` → `Suppressed`, no offer.
- [x] BR-2 / AC-2: `plan` → `Suppressed { DeniedLevel }` with one event and no gate call; `full`
      → written with no prompt; `generate = always` at `guarded` → written with no prompt and the
      event says `always`; `generate = never` → `Suppressed`, and `Init` still runs.
- [x] BR-8: `Init { force: false }` with a file present → `Failed { AlreadyExists }` naming the
      size and `--force`; `Init { force: true }` at `guarded` raises the `replace` question.
- [x] The offer never runs mid-turn: a two-iteration tool loop sees no second offer.
- [x] `cargo test -p tetond --no-fail-fast` green; every Verification row below resolves to a
      real, executed case.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/tests/repo_context_generation.rs::the_offer_is_raised_once_per_session_per_root_on_the_first_prompt_and_never_after_a_decline` | yes |
| BR-2 | test-case | `crates/tetond/tests/repo_context_generation.rs::plan_suppresses_without_a_prompt_full_and_always_write_without_one_and_never_suppresses` | yes |
| BR-8 | test-case | `crates/tetond/tests/repo_context_generation.rs::init_refuses_an_existing_file_without_force_and_asks_the_replace_question_with_it` | yes |
| BR-10 | test-case | `crates/tetond/tests/repo_context_generation.rs::plan_suppresses_without_a_prompt_full_and_always_write_without_one_and_never_suppresses` | yes |
| AC-1 | test-case | `crates/tetond/tests/repo_context_generation.rs::the_offer_is_raised_once_per_session_per_root_on_the_first_prompt_and_never_after_a_decline` | yes |
| AC-11 | test-case | `crates/tetond/tests/repo_context_generation.rs::plan_suppresses_without_a_prompt_full_and_always_write_without_one_and_never_suppresses` | yes |

## Technical Notes

The state is written under the sessions lock and the gate is awaited outside it (the
`set_session_cwd` discipline). The hook runs on the claiming turn only.

### As built (2026-09-04)

- `store_session_repo_context` and `session_context` live in `runtime/mod.rs`, not
  `runtime/session.rs`; both were edited where they are.
- `session/context` moved to `handle_client`'s `blocks_on_a_human` task: `Init` raises a
  permission prompt and then spends a model call, so the method may no longer answer from the
  connection's reader loop. `DaemonRuntime::session_context` is `async` and takes the asking
  connection as the offer's addressee.
- `RepoContextState::Absent` cannot decide the arming on its own — an empty `TETON.md` loads as
  `Absent` by REQ-612's rule and BR-1 counts it as *present* — so the arming stats the two
  candidate names through the injected reader (`generate::notes_present`), and only when the
  notes switch is on.
- The record carries the root the state was decided at beside the state, so BR-1's "per root"
  is structural: a re-derivation at the same root never re-arms a decision, and a `/cd` always
  does.
- A **skill** turn does not raise the offer (`skill_turn.is_none()`), on
  `spawn_title_session`'s reason: REQ-589 BR-8's refusal says nothing was sent and no provider
  saw the turn, and a draft call spent in `assemble` would make that false. The state stays
  `Pending` for the next typed prompt.
- `draft_route` narrowed to `pub(super)` (not `pub(crate)`: that spelling trips
  `runtime_visibility`'s crate-wide ratchet instead of its `pub` one) and now answers a
  `DraftPlan` — the route and the tier from **one** resolution, since the header line and the
  event both need the tier.
- The offline `ScriptedFileEngine` recognizes the draft contract on its own 2,048-byte window:
  the draft instruction is the longest in the harness and states its contract ~1,300 bytes in,
  past `DUTY_CONTRACT_PREFIX_BYTES`.
- Fixture fallout, fixed at the source: the shared e2e demo repo and `skill_turn`'s tree now
  plant an **empty** `TETON.md` (BR-1's own documented opt-out), because every scripted fixture
  in a project root would otherwise draw a prompt nobody answers or spend a scripted reply.

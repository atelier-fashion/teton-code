---
id: TASK-296
title: "Migrate the turn path to TurnContext, constructed after the last rebinding"
status: complete
parent: REQ-598
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-294]
---

## Description

Construct `TurnContext` once in `run_prompt_turn` and thread it to
`build_tools`, `offer_or_refuse_over_budget`, and `run_one_attempt`.

**This is the task where a behavior change is most likely to enter the REQ**,
and the hazard is the construction *point*, not the threading. See ADR-4 and
BR-2.1.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — `run_prompt_turn` (construction),
  `build_tools`, `offer_or_refuse_over_budget`, `run_one_attempt`

## Acceptance Criteria

- [ ] `TurnContext` is constructed in `run_prompt_turn` **after the REQ-580
      warming hold's `match` resolves** — i.e. after the point where `router` is
      shadow-rebound — not merely after the claim. BR-2.1.
- [ ] `build_tools` takes `(&self, tctx, skills, invoker)` and carries **zero**
      suppressions afterwards.
- [ ] `offer_or_refuse_over_budget` takes the context plus `route`, `stage`,
      `skill`, `system`, `already_accepted`. If it still exceeds the threshold,
      move `invoker` into `TurnContext` (it is a per-turn fact) rather than
      keeping a suppression — and if that is done, do it in `TurnContext`'s
      definition, not as a local workaround.
- [ ] `run_one_attempt` takes the context and builds its `DutyContext` from it
      after the one `local_engine` slot read. `route` remains a parameter (ADR-3).
- [ ] BR-7: `build_tools` builds the `ToolRegistry`, and the cap-exempt
      (mandatory) versus optional tool distinction must stay **visible** after
      the migration — not absorbed into the context where a reader stops seeing
      it. LESSON-496 is what this guards: "cut first under pressure" became
      "never available" when the limit equalled the mandatory count. Confirm the
      ordering-dependent registry logic reads the same way it does now, and that
      no part of the cap decision moves into `TurnContext`.
- [ ] BR-9: no new suppression of any kind is introduced to make this compile.
- [ ] BR-1: TASK-293's fixture test still passes unchanged.
- [ ] BR-5: no `TurnContext` construction is inserted between a security gate
      and the parse it guards. Verify by reading each gate site touched, not by
      assuming.
- [ ] BR-8: the REQ-580, REQ-567, REQ-583, REQ-585, REQ-589 and REQ-572 rationale
      comments in the construction region stay attached to the code they explain.
- [ ] `cargo clippy --workspace --all-targets` clean; `cargo test --workspace
      --no-fail-fast` green with output grepped for `FAILED`.

## Technical Notes

The ordering in `run_prompt_turn` that must be preserved exactly:

1. `_claim` — before any of the turn's work (REQ-567 BR-5 / D-3)
2. `session_cwd` re-read from the registry (REQ-583, LESSON-539)
3. `config` — one snapshot, taken before the expansion because the gate reads it
4. `gate` — fetched, not built (a rebuilt gate forgets session grants)
5. `skills` — the turn's one registry snapshot
6. `accept_invocation` — the expansion, which needs the gate
7. `router` — first binding
8. **the warming hold — which may rebind `router` and re-dispatch `route`**
9. **← construct `TurnContext` here**

Steps 3–6 are ordered by REQ-589 ADR-10 and REQ-585 BR-4/ADR-3, and each has a
comment saying so. Constructing the context earlier to "tidy" the function is
the failure mode this task exists to avoid: it would satisfy BR-2, pass the
existing suite, and break REQ-580.

`run_prompt_turn` keeps its own suppression. It is the constructor site and its
parameters arrive off the wire; there is no bundle to collapse.

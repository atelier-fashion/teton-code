---
id: TASK-426
title: "SharePool: equal split of prompt headroom, rising ceilings, release on terminal status"
status: complete
parent: REQ-623
created: 2026-10-05
updated: 2026-10-05
dependencies: ["TASK-424"]
repo: teton-code
---

## Description

ADR-4. A per-call pool created from `headroom = ceiling − parent accumulator` and `n`;
`share_of(child)` is read **at check time** by the child's egress so a raised ceiling is seen;
`release(child)` splits an ended child's unspent share equally among running children (floor,
remainder unused) and returns the recipients for `agent_child_share_released`. Every child
record also adds into the parent's accumulator so the parent's next call checks real headroom
on the existing `SpendCeilingReached` arm. A prompt with no ceiling yields a pool that never
refuses and never releases.

## Files to Create/Modify

- `crates/tetond/src/cost/share.rs` — new: `SharePool`, `ChildShare`, pure arithmetic + a `Mutex` for release; table-driven unit tests
- `crates/tetond/src/cost/mod.rs` — export
- `crates/tetond/src/egress/mod.rs` — a child egress reads its ceiling through `Arc<SharePool>` and writes spend to both its own and the parent accumulator

## Acceptance Criteria

- [x] `headroom 1000, n 3` → shares `333/333/333`; one child ends having spent 100 → the two others gain `116` each (`floor(233/2)`), remainder 1 unused
- [x] A child whose next call would exceed its current share gets `SpendCeilingReached` from its egress; a sibling with raised share does not
- [x] After a call, the parent's accumulator equals its own spend plus every child's
- [x] No ceiling → `share_of` is `None` and `release` is a no-op
- [x] Mutation recorded: remove the release and name which test reddens

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/src/cost/share.rs::tests::release_splits_unspent_equally_floor` | yes |
| BR-7 | test-case | `crates/tetond/src/cost/share.rs::tests::ceiling_only_rises` | no |

## Technical Notes

- Keep the arithmetic a pure function (`split(headroom, n)`, `release(unspent, running)`) and test it table-driven; the `Mutex` wrapper only sequences calls.
- LESSON-552: the e2e AC-13 test (TASK-430) drives the derivation from the ledger; this task pins the arithmetic.
- The parent accumulator is `Arc<PromptSpend>` (`teton_core::cost_ceiling`), created once per prompt in `run_prompt_turn` — children are handed the same `Arc`, plus their own.
- LESSON-557: no new typed outcome — the child's refusal *is* `SpendCeilingReached`, composed by `teton_core::cost_ceiling` as today.

## Implementation Notes

- `SharePool::new(headroom: Option<u64>, children: &[ChildId]) -> Arc<SharePool>`,
  `share_of(&ChildId) -> Option<u64>` (a child the pool does not know gets `Some(0)` in a
  limited pool — fail closed), `release(&ChildId, spent) -> Vec<(ChildId, u64)>`. Pure
  arithmetic: `headroom`, `split`, `unspent`, `release_parts`.
- `ChildSpend::new(child, pool, parent: Option<Arc<PromptSpend>>)` is the child's wiring;
  `Egress::with_child_spend(Option<ChildSpend>)` installs it. A child's choke point checks
  its own accumulator against `share_of` at every send through the same
  `refuse_past_ceiling` body (and composer) the prompt pair uses; the prompt pair is not
  consulted when a `ChildSpend` is installed. The meter is handed a per-call accumulator
  and the body is wrapped so its cost is forwarded to the child's and the parent's — the
  `CostMeter` trait and the ledger are unchanged.
- Mutations (full `tetond` lib suite): `release` raising nobody → 5 red
  (`release_splits_unspent_equally_floor`, `ceiling_only_rises`,
  `a_release_never_mints_spend`, `a_child_releases_once_and_a_stranger_not_at_all`,
  `egress::tests::a_child_over_its_share_is_refused_while_a_raised_sibling_is_not`);
  ender counted as running → the same 5; remainder to first recipient → 2; plain `-` in
  `unspent` → 3; ended children still receiving → 4; child check reading the parent → 1;
  no parent add → 3; forward before dropping the inner body → 1; no forward on the
  terminal `None` → 2; prompt pair checked on a child's choke point → 1; unknown child
  answered `None` → 1. Each test's doc comment names its reds.

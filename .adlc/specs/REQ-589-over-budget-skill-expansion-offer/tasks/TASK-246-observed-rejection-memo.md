---
id: TASK-246
title: "Session-scoped memo of observed window rejections"
status: complete
parent: REQ-589
created: 2026-08-24
updated: 2026-08-24
dependencies: []
---

## Description

BR-14.2 / ADR-9. Remember that this skill on this route was actually rejected at the window, so the next offer is better informed. This is an observation, NOT a consent.

## Files to Create/Modify

- `crates/tetond/src/runtime.rs` — a store shaped exactly like `EffortRefusals` (551), keyed by (SessionId, skill, route)

## Acceptance Criteria

- [x] Session-scoped and never persisted to disk
- [x] `mark()` returns the first-time transition so the caller can announce once
- [x] The record does NOT suppress the next offer and does NOT pre-answer it — two negative assertions (AC-23, BR-10 boundary)
- [x] The record lives in ONE store, daemon-side; the CLI does not memoize it (ASSUME-017)
- [x] A resident system-prompt fact — DISCHARGED by TASK-258 (`31d7f15`) states that consents are not persisted and observations are, so the model cannot claim it 'remembers' a consent (LESSON-543) — **deferred, see below**

## Technical Notes

`EffortRefusals`' doc comment already says 'Remembering is not retrying' — mirror that framing, it is the exact distinction BR-10 turns on.

## Implementation notes

`ObservedWindowRejections` + `RouteWindow` live beside `EffortRefusals` in
`runtime.rs`, keyed by `(SessionId, skill, RouteWindow)`. `RouteWindow` is read
off the stamped `RouteBudget` — `bound`, `provider_id`, `budget_tokens`,
`budget_bytes` — and the **figures are deliberately part of the route's
identity**: raising the window is the remedy the offer proposes, so a remedied
route is a different route and the observation made under the old window does
not carry across it. Keying on the provider alone would replay a stale record,
which is ASSUME-017's harm in a different hat.

Five tests in `runtime::tests::observed_window_rejections`, including the two
negative assertions (`..._does_not_suppress_the_next_offer` compares the
`skill_fit` verdict byte-for-byte either side of a `mark`;
`..._does_not_pre_answer_the_next_offer` asserts the session's `PermissionGate`
holds no answer under either skill key) and the LESSON-508 seam test
(`..._never_reaches_disk_and_is_gone_on_restart`, both halves: nothing under the
state dir names the skill, and a `from_env` restart over that dir has forgotten
it).

### The resident prompt fact is reported, not written

The resident-facts block is `crates/tetond/src/harness/self_config.md`, pinned by
whole-line assertions in `harness/turn_loop.rs` (`:4103`, `:4135`, `:4212`,
`:4284`) and cross-read by `crates/teton/src/cli_rows.rs`'s `guide_tests`. None
of those files is in this task's ownership, and no other REQ-589 task claims
`self_config.md` — **this AC currently has no owner**. What the fact must say:

> Teton never remembers your answer to a permission question — an approval is
> for that one turn only, and the next one asks again. It does remember what it
> *observed*: a skill a provider already refused as too large is named as such
> when the same skill is offered again on the same route. Never say you remember
> an approval.

Both halves are load-bearing (LESSON-543: name the negative space, not only the
roster) and the sentence must be pinned in parts — the "asks again" clause and
the "observed, not approved" clause asserted separately — so a later REQ
re-words it instead of deleting it. Note the budget constraint the lesson
records: the resident prompt is floor-guarded (`MIN_PROMPT_HEADROOM_BYTES`),
so the owning task must measure headroom first and pay for the sentence by
shortening another.

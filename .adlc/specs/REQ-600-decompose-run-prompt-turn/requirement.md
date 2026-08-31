---
id: REQ-600
title: "Decompose run_prompt_turn into a stage sequence, and slice the god-impl"
status: draft
deployable: true
created: 2026-08-31
updated: 2026-08-31
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["developer-experience", "reliability", "extensibility"]
tags: ["refactor", "god-impl", "run-prompt-turn", "turn-path", "req-599-followon"]
---

## Description

REQ-599's deferred step 8, filed as its own REQ by decision on 2026-08-30.

REQ-599 delivered seven seams and took `runtime.rs` from 14,183 production lines
to 10,306 in `mod.rs`. It did **not** reach its own AC-1 target ("no module
above 2,000, `mod.rs` under 1,000"), and the arithmetic says why: six of its
seven steps moved *top-level* items — types, free functions, constants — and
only one took methods out of `impl DaemonRuntime`. That impl is still **~6,540
production lines**, and it is the whole remaining problem.

Within it, `run_prompt_turn` is ~1,084 lines carrying session claiming, skill
expansion, routing, budget checks, consent, dispatch and commit in one `async
fn`.

**Why this was split off rather than done.** REQ-599's steps *relocate* code;
this one *changes control flow* on the path every prompt runs through. With tests
at 61% of the file, landing them together buries the one genuine behavior risk
inside a diff several times its size. This REQ's diff should be readable as a
change, because that is what it is.

## Inherited from REQ-599

- **AC-2**: `run_prompt_turn` reduced to a stage sequence, body under 200 lines,
  each stage independently nameable and testable.
- **AC-3**: `turn_loop.rs::run_session_turn_with_pressure_policy` (762 lines,
  8–9 levels of nesting) drops to depth 5 or below. It was REQ-599's OQ-3 and
  sits in a different module with a different parameter cluster (REQ-598 ADR-2).
- **BR-3's ordering invariants**, which are the actual risk: the three
  typed-outcome arms stay ordered before the generic remote arm; gates stay
  before the parses they guard (LESSON-520); the claim stays before the
  session-state read (LESSON-539); presence gates keep their reader-loop freedom
  (LESSON-518, BUG-184); and the `TurnContext` construction point stays after
  the REQ-580 warming hold (REQ-598 BR-2.1).

## What REQ-599 learned that this REQ should start from

- **Rationale ids are a weak positive signal — propose with them, decide with
  structure** (LESSON-593 as corrected 2026-08-31, REQ-599 ADR-1's correction
  block). The original guidance here read "1 of 19 clustered, 13 scattered file-
  wide. Do not re-propose that method." **That was wrong and is retracted.**
  The 1-of-19 figure came from a `max − min` span with no breakdown resistance;
  one outlying annotation forces the "scattered" verdict. Under the smallest
  window holding 70% of an id's items, **5 of 19 cluster** — and REQ-581's
  219-line window is exactly the set that became `runtime/provider.rs`, a seam
  REQ-599 skipped on the strength of the bad statistic.
  What genuinely fails is the requirement's *literal rule* ("where they
  interleave across a proposed boundary, the boundary is wrong"): in a
  cross-cutting file that condemns every boundary, so it cannot choose one. Use
  ids to **generate candidates**, structure to **decide**, and if you measure
  locality, use a densest-window or quantile rather than a range.
- **Seams are created, not only discovered** (REQ-599 step 7). `provider`
  measured as scattered across 10,366 lines at Phase 2 and was 375 contiguous
  lines after four unrelated slices left. Re-measure cohesion after each step
  rather than planning the whole order up front.
- **Adjacency is not membership.** Two items inside extraction ranges belonged
  elsewhere and were checked rather than assumed.
- **Every derived check is part of the change** (LESSON-594). Seventeen
  source-scanning tests broke across REQ-599's first two steps, one of them from
  a `#[cfg(test)] mod` declaration placed at the top of a file.
- **Visibility passes can narrow the API** (LESSON-595). Re-export by name.

## Baseline, measured 2026-08-31 at `b3c2a80`

Every figure states its rule. This REQ line has produced five wrong answers to
one question by pairing a count with the wrong rule (LESSON-593, LESSON-597), so
no bare number appears below.

| quantity | rule | value |
|---|---|---:|
| `runtime/mod.rs` production | lines above the first **column-0** `#[cfg(test)]` | 10,306 |
| `impl DaemonRuntime` | the three `impl … DaemonRuntime` blocks, production only | 6,543 |
| — the inherent block alone | `mod.rs:2152..8684` | 6,533 |
| its methods | 89 method bodies, signature to closing brace | 4,618 |
| — the rest | doc comments and blank lines between methods | 1,915 |
| `run_prompt_turn` | body span, signature line to closing brace | 1,084 |
| `run_session_turn_with_pressure_policy` | body span | 762 |
| — its nesting | **max brace nesting inside the fn** | **9** |
| — the same, other rule | indentation levels below the `fn` | 11 |

The two nesting rules disagree by 2 against a target of 5, which is why AC-3 now
names the rule it means. The description's "8–9 levels" was the brace rule; it
was never stated as such.

Six methods are ≥200 lines and account for **51%** of the impl's 4,618 method
lines: `run_prompt_turn` (1,084), `derive_provider_setup` (324),
`provider_test_within` (310), `offer_or_refuse_over_budget` (249),
`run_one_attempt` (207), `accept_invocation` (206). Only the turn-path ones are
in scope; the two provider methods are excluded by Out of Scope below.

## Acceptance Criteria

- [ ] AC-1: `run_prompt_turn`'s body is under **200 lines**, a sequence of named
      stages. **Rule: body span, the `fn` signature line through its closing
      brace, as measured in the baseline table** — the same rule that reports it
      at 1,084 today. Doc comments inside the body count; comments above the
      signature do not.
- [ ] AC-2: `impl DaemonRuntime`'s production line count drops to **4,500 or
      below** (from 6,543), under the baseline table's rule.
      **The target is fixed here, not in the architecture doc.** An AC whose
      threshold is chosen later, in a document the implementer also writes, after
      seeing the result, is not falsifiable. `/architect` may *tighten* this
      number; loosening it requires recording the reason in the architecture doc
      and saying so in the PR body, so a missed target reads as a missed target.
- [ ] AC-3: `run_session_turn_with_pressure_policy`'s maximum nesting depth is
      **5 or below under the brace-nesting rule** — currently 9 — and the
      indentation-rule figure is recorded alongside it, currently 11. Both are
      reported; the brace rule is the one that gates.
- [ ] AC-4: Every BR-3 ordering invariant above has a test that fails when the
      ordering is inverted — not a comment asserting it holds. Each inversion is
      **run and its failure recorded**, per REQ-602's finding that a mutation
      whose outcome was never observed had in fact been impossible.
- [ ] AC-5: The REQ-598 event-ordering fixture replays identically. The fixture
      is **not regenerated**: a golden file computed by its own subject is not an
      oracle (LESSON-569).
- [ ] AC-6: REQ-599's traceability sweep and module-map guard both still pass,
      and `runtime_visibility.rs` and `runtime_doc_paths.rs` (REQ-602) pass over
      whatever modules this REQ adds — they enumerate their corpora from disk
      precisely so a new module is scanned rather than silently exempt.
      **`BASE` and `TOUCHED` are deliberately NOT repointed.** This criterion
      originally said to repoint them; REQ-602 recorded that same instruction as
      deliberately not done, and the reason applies unchanged here: `BASE` is
      REQ-599's pre-split commit `17c39ec`, and repointing it at a post-split
      base makes the sweep compare the split tree against itself, which proves
      nothing about the split. The wording was inherited from REQ-599's template
      without the correction.
- [ ] AC-7: Delivered as independently-green commits, each reviewable as a
      change rather than as a relocation. **Green means every required check on
      that commit reports success, not "was not cancelled."** REQ-599's identical
      criterion was marked NOT MET because CI's `cancel-in-progress` cancels the
      previous commit's still-running `macos-latest` job the moment the next step
      is pushed — and macOS is the runner that caught LESSON-591's race. Either
      each step's CI is allowed to finish before the next is pushed, or the
      criterion is recorded NOT MET again with the same cause. It is not met by
      pushing seven commits and reading the last one's status.

## Assumptions

- **The ordering invariants in BR-3 are the whole behavioural risk.** Everything
  else here is relocation and renaming. If an invariant turns out not to be
  testable by inversion (AC-4), that is a finding to record, not a criterion to
  quietly drop.
- **`run_prompt_turn`'s stages are separable at all.** The description asserts
  seven concerns in one `async fn`; whether they factor into stages with a
  nameable boundary is a claim this REQ must confirm before committing to a
  shape, and say so plainly if they do not. Cohesion is re-measured after each
  step rather than planned up front (REQ-599 step 7 — `provider` measured as
  scattered across 10,366 lines and was 375 contiguous lines once four unrelated
  slices had left).

## Out of Scope

- Further module extraction that does not serve the decomposition of the turn
  path. REQ-599's seven seams are enough surface to work against. Concretely,
  this excludes `derive_provider_setup` (324 lines) and `provider_test_within`
  (310) — the second and third largest methods in the god-impl — because neither
  is on the turn path. They are named here so their exclusion is a decision
  rather than an oversight.
- **The session-lifecycle slice (REQ-603).** It cuts the same `impl
  DaemonRuntime` and would collide. This REQ lands first; REQ-603 re-measures
  after.

## External Dependencies

- None. Everything is inside `crates/tetond`.

## Open Questions

- [ ] OQ-1: Do the stages become free functions taking `TurnContext`, methods on
      a `TurnStages` type, or an enum-driven sequence? REQ-598's ADR-3 argued
      against a typestate for `route`; the same argument may or may not apply to
      the stage sequence. **`/architect` must answer this**, since it decides
      the diff's shape.
- [x] OQ-2: **Answered — `turn_loop.rs` stays in this REQ.** It was contradictory
      to hold this open while AC-3 already made the file in scope. It shares
      BR-3's ordering invariants, which are the risk this REQ exists to manage,
      and splitting the depth fix from the stage extraction would mean two
      changes to the same call path reviewed apart. If it proves separable
      cleanly, `/architect` may sequence it as its own commit under AC-7 — that
      is a commit-boundary decision, not a scope one.

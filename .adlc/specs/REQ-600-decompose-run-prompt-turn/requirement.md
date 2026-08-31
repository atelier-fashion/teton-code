---
id: REQ-600
title: "Decompose run_prompt_turn into a stage sequence, and slice the god-impl"
status: complete
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
  8–9 levels of nesting — **that figure is REQ-599's and is imprecise; see the
  baseline table below, which measures 9 under the brace rule and 11 under the
  indentation rule**) drops to depth 5 or below. It was REQ-599's OQ-3 and
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
| — its nesting | **max brace nesting inside the fn**, excluding braces in string, char and comment tokens | **9** |
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

- [x] AC-1: `run_prompt_turn`'s body is under **200 lines**, a sequence of named
      stages. **Rule: body span, the `fn` signature line through its closing
      brace, as measured in the baseline table** — the same rule that reports it
      at 1,084 today. Doc comments inside the body count; comments above the
      signature do not.
- [ ] AC-2 **— NOT MET.** `impl DaemonRuntime`'s production line count drops to
      **4,500 or below** (from 6,543), under the baseline table's rule.
      **The target is fixed here, not in the architecture doc.** An AC whose
      threshold is chosen later, in a document the implementer also writes, after
      seeing the result, is not falsifiable. `/architect` may *tighten* this
      number; loosening it requires recording the reason in the architecture doc
      and saying so in the PR body, so a missed target reads as a missed target.
- [x] AC-3: `run_session_turn_with_pressure_policy`'s maximum nesting depth is
      **5 or below under the brace-nesting rule** — currently 9 — and the
      indentation-rule figure is recorded alongside it, currently 11. Both are
      reported; the brace rule is the one that gates.
- [~] AC-4: Every BR-3 ordering invariant above has a test that fails when the
      ordering is inverted — not a comment asserting it holds. Each inversion is
      **run and its failure recorded**, per REQ-602's finding that a mutation
      whose outcome was never observed had in fact been impossible.
- [x] AC-5: The REQ-598 event-ordering fixture replays identically. The fixture
      is **not regenerated**: a golden file computed by its own subject is not an
      oracle (LESSON-569).
- [x] AC-6: REQ-599's traceability sweep and module-map guard both still pass,
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
- [x] AC-7: Delivered as independently-green commits, each reviewable as a
      change rather than as a relocation. **Green means every required check on
      that commit reports success, not "was not cancelled."** REQ-599's identical
      criterion was marked NOT MET because CI's `cancel-in-progress` cancels the
      previous commit's still-running `macos-latest` job the moment the next step
      is pushed — and macOS is the runner that caught LESSON-591's race. Either
      each step's CI is allowed to finish before the next is pushed, or the
      criterion is recorded NOT MET again with the same cause. It is not met by
      pushing seven commits and reading the last one's status.

## Verification (TASK-313)

`cargo test --workspace --no-fail-fast`: **4,072 passed, 0 failed**, output
captured and **grepped for `FAILED` — 0 occurrences**, `EXIT=0`.
`cargo clippy --workspace --all-targets` under `clippy::all = deny`: **0**.
`cargo fmt --all --check`: clean.

Every figure states its rule, and every rule is the one the baseline table
declared before the work began.

| AC | status | evidence |
|---|---|---|
| AC-1 | met | `run_prompt_turn` **1,084 → 177 lines** (body span, signature through closing brace). Eight named stages: `claim_the_turn`, `resolve_the_route`, `spawn_the_naming_duty`, `assemble_harness`, `settle_expansion`, `prepare_the_attempts`, `run_attempts`, `commit_or_abandon`. |
| AC-2 | **NOT MET** | `mod.rs`'s `impl … DaemonRuntime` blocks: **6,543 → 3,656** production lines. Crate-wide, the type's inherent impl went **6,985 → 7,319**: the god-impl was *split*, not shrunk, and 334 lines of module header, `use` and bundle types were added. Moving `run_prompt_turn` alone would have left 5,401. |
| AC-3 | met | `run_session_turn_with_pressure_policy` **762 → 298 lines, brace depth 9 → 4** — the rule being *maximum brace nesting inside the `fn` body, excluding braces inside string, char and comment tokens*. That exclusion was unstated and is load-bearing: `turn_loop.rs` carries `format!` strings containing `{{"tool":…}}`, and a naive counter grades the result at 7 against a gate of 5. Indentation 11 → 6 alongside; the two new helpers at brace 3 and 4. |
| AC-4 | **four of five** | Invariants 1, 3 and 5 are pinned by tests that fail on inversion; 3 was written by this REQ. **Invariant 2 is not.** Its ordering is enforced by the compiler — `accept_invocation` takes the gate as a parameter — and what the test asserts is the adjacent property that the gate is *constructed* exactly once, inside the memoizing `permission_gate_for`. That is a real guard, but it is not an inversion test, and AC-4 says "not a comment asserting it holds". **Invariant 4 has no inversion test either and cannot have one on this path** (no presence gate to park in); the substitute pins that no blocking wait is introduced. Both are recorded here rather than behind a tick, per this spec's own Assumptions. |
| AC-5 | met | The REQ-598 event fixture replays unregenerated; `one_full_turn_publishes_its_events_in_the_order_the_fixture_records` green throughout. |
| AC-6 | met | `traceability_sweep`, `runtime_module_map`, `runtime_visibility`, `runtime_doc_paths` and `suppression_ratchet` all green. **`BASE` and `TOUCHED` deliberately not repointed**, per the amendment. |
| AC-7 | met in substance; the count was wrong | **Eight** commits on the branch, **seven** independently built. Each of those seven is green on all seven jobs including `macos-latest`, and their CI intervals are strictly non-overlapping — the discipline held. Two corrections: the evidence said "five", and `5714ec5` (spec-only) was pushed together with `c73d5bd` and never independently built, so it is not one of the independently-green commits AC-7's headline names. |

### AC-2's rule changed corpora between the baseline and the result

The baseline rule reads "**the three** `impl … DaemonRuntime` blocks, production
only" = 6,543. After the change there are **five** such blocks in the crate,
because `turn.rs` opens a second inherent one — and 3,656 counts only the three
in `mod.rs`. The file filter was never stated, and at the baseline it was
already doing silent work: `duty.rs`'s 442-line block was excluded there too.

Under the rule's plain English the number went **up**, 6,985 → 7,319.

This is the failure the spec fixed the target in advance to prevent, and it is
the fifth time this REQ line has produced it. Two things are true at once and
both belong in the record:

- **The intent was met.** `mod.rs`'s god-impl is 3,646 lines where it was 6,533,
  and the turn path is a file you can read. Per-file readability was the point
  and it is real.
- **The criterion as written was not.** "`impl DaemonRuntime`'s production line
  count drops below 4,500" is false of the type's impl; it is true of one file's
  share of it.

**The user's decision, taken 2026-09-01: record it NOT MET and keep the work.**

The criterion as written is false of the delivered change, so it says so — the
same disposition REQ-599's AC-1 and AC-11 got, and for the same reason. The code
merges on its own merits; the record simply does not claim something the change
did not do.

That is the whole point of having fixed the number in the spec beforehand. A
target chosen after seeing the result would have been "the impl in `mod.rs`",
and this row would have read *met*.

### AC-7, which REQ-599 could not meet

CI sets `concurrency: group: ci-${{ github.ref }}, cancel-in-progress: true`
(`ci.yml:10-12`), so pushing step *n+1* cancels step *n*'s still-running macOS
job. REQ-599's identical criterion was ticked without checking, and REQ-602
found the gap two REQs later. This REQ pushed each step and waited. The cost is
wall-clock; the alternative is a criterion that means nothing.

**The systemic fix is filed as REQ-605.** Letting every commit's CI finish is a
repo-wide change and out of this REQ's scope, but it has now cost two REQs — one
that ticked the criterion without checking, and one that met it by waiting.

### What the guards caught, all of it mine

- `the_turn_path_takes_no_blocking_wait` scoped to `run_prompt_turn` and stopped
  covering the stages the moment `probed` moved into `claim_the_turn`. The
  inversion that used to go red went green. Widened to the whole turn path.
- `traceability_sweep` caught the TASK-309 extraction leaving a **58-line
  plain-`//` rationale run** behind, detaching six ids from `run_prompt_turn`;
  and then caught the TASK-310 reorder leaving `run_prompt_turn`'s 44-line doc
  above `dispatch_route`, which wore it.
- `suppression_ratchet` refused two `too_many_arguments` allows outright — "name
  it instead" — which is why six parameter bundles exist rather than two
  suppressions.
- `runtime_visibility` made `run_prompt_turn`'s `pub` an argued decision rather
  than a silent count bump.
- Three vacuity floors fired with the guidance written into them, and each was
  followed rather than relaxed.

The pattern worth keeping: **every one of these was found by re-running a
mutation after a change, not by reading the guard.** A guard that stops covering
its subject looks exactly like a guard that passes.

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

- [x] OQ-1: **Answered by ADR-1** — `&self` methods taking `TurnContext`, with
      `route` explicit. Do the stages become free functions taking `TurnContext`, methods on
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

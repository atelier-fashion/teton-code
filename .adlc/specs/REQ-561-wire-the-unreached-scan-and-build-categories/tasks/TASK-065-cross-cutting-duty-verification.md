---
id: TASK-065
title: "Cross-cutting verification — the duty matrix, egress capture, taint, ceilings, mutations"
status: complete
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: [TASK-064]
---

## Description

The acceptance criteria that span all five duties and cannot be written until
they all exist: the failure matrix (AC-3), egress capture with non-vacuity
(AC-4), the taint override (AC-5), the ceiling assertions (AC-11), the call-site
tagging assertion (AC-10), the seam assertion (AC-8), and the mutation checks
(AC-9).

Each duty task already carries its own scripted-engine test (AC-12) and its own
degradation test, so this task is the cross-product, not a re-run.

## Files to Create/Modify

**Amended during implementation.** The three planned files sat under `tests/e2e/`,
which spawns the real daemon binary. Two of the three claims are not observable
from outside the daemon and one was already fully covered, so the layout landed
as below. Each deviation is a decision, not a shortcut, and each is recorded.

- `crates/tetond/tests/duty_matrix.rs` — **new**, and a **library-level**
  integration test rather than an e2e one. AC-3: all five duties × four
  conditions, driven through the **real call sites** (`summarize_if_large`,
  `ToolRegistry::refine`, `name_session`, `compact_if_pressured` + the hard
  gate). `compact`'s invariant is "the context is under budget afterwards",
  which no protocol client can see; and 20 daemon spawns would be a slow,
  flaky way to assert something the call sites answer directly.
  **The fourth condition is stated precisely**: a call site cannot see a
  session, so a tainted session reaches it either as a local/unresolved route
  (rows 1–2) or as content its own choke point refuses (row 4). The
  resolver-side half is asserted where the resolver lives.
- `crates/tetond/tests/duty_egress.rs` — **new**, library-level. AC-4's
  **scoping** half (BR-7): a duty whose own content is clean sends while the
  turn's wider context carries boundary material, with the control leg showing
  the same route refuses that turn's own provenance. AC-4's capture-plus-
  non-vacuity half already exists per duty and is not duplicated.
- `crates/tetond/tests/e2e/duty_taint.rs` — **new**, e2e (registered in
  `tests/e2e.rs`). AC-5, by captured bytes: duties bound to a *separate* mock
  provider from the turns, a tainted session sending zero, and an untainted
  control on the same daemon sending both duty prompts.
- `crates/tetond/tests/e2e/duty_ceilings.rs` — **not created**. AC-11 is already
  covered per duty by five `a_remote_*_is_bounded_however_much_the_provider_streams`
  tests that read each declared constant. A sixth copy would be duplication;
  the cross-cutting half (all five declare one, and the five are ordered) is in
  `harness::duty`'s tests instead.
- `crates/tetond/src/harness/duty.rs` — AC-8/AC-10 assertions (source-level
  scan) and the `# What breaks which test` mutation table at the module head.
- `crates/tetond/src/call_sites.rs` — the source-scanning helpers extracted into
  a shared `scan` module, so the derived-marker test and the seam assertions
  read production source by one rule rather than two.
- `crates/tetond/src/harness/{triage,shell_duty,compact}.rs` — ceiling
  derivation/band pins, closing the disclosed AC-11 limitation as far as each
  duty's own rationale allows (see "Findings" below).
- `crates/tetond/src/harness/context.rs` — one added assertion, the artifact of
  the green mutation below.

## Acceptance Criteria

- [x] **AC-3**: the table-driven matrix covers all five duties × four conditions. For `compact`, "the invariant holds" means the context is under budget afterward. — `tests/duty_matrix.rs`, plus a non-vacuity test asserting each duty's `Resolves` row genuinely changes its call site's outcome, plus a per-row assertion that each failure fails for *its own* reason (an unresolvable route carries the resolver's sentence; the refused row names the boundary; the provider-error row does not).
- [x] **AC-4**: for each duty, with a remote binding and a `local-only` boundary, **zero** boundary bytes appear in any captured payload — **paired with a non-vacuity assertion** that the same duty *does* send when the content is clean. — **already shipped per duty** by TASK-058/060–063 (`a_local_only_*`/`a_*_refused_*` each paired with `a_remotely_bound_*_sends_*` or `an_unbounded_machine_sends_*`). Not duplicated; audited and confirmed.
- [x] **AC-4 (BR-7 scoping)**: a duty whose *own* content is clean still sends while the turn's wider context contains boundary material. — `tests/duty_egress.rs`, for `digest`, `triage` and `title`. `compact` and `shell` **cannot** participate and each is asserted separately with its own claim and its own reason (see Findings).
- [x] **AC-5**: a tainted session runs all four new duties on the local tier regardless of binding, asserted by **captured bytes**, not by reading the resolved route. — `tests/e2e/duty_taint.rs` for `title` and `triage` by bytes, and for `shell` by the local answer reaching the reply. `compact` is out of that fixture's reach (see Findings).
- [x] **AC-10**: a source-level assertion that no duty's category is produced from prompt text, tool name, or any string comparison. — `harness::duty::tests::no_duty_category_is_ever_produced_from_text`, plus `the_only_text_to_category_map_is_the_ledgers_own_round_trip` for the one stated exception.
- [x] **AC-8**: the seam assertion per the architecture's five-point boundary. — three tests in `harness::duty::tests`, one per pair of points.
- [x] **AC-9**: for each duty, (a) removing the taint override and (b) making the failure path return its input unchanged each turn **at least one test red**. Each mutation applied and observed, then reverted. — 16 mutations applied, compiled and observed (15 red, 1 green); the table is at `harness/duty.rs`'s module head.
- [x] **A mutation that comes back green is reported as a finding, not quietly fixed** (LESSON-485). — one green mutation found, analysed and recorded (see Findings). It is an *equivalent* mutant within the loop, and the analysis is written down rather than the test bent to catch it.
- [x] `cargo test --workspace --no-fail-fast` is green. — **1243 passed, 0 failed, 1 ignored across 36 targets** (baseline 1227/0/1/34). `cargo clippy --workspace --all-targets` and `cargo fmt --check` both clean.

## Findings

1. **One green mutation.** Making the turn loop's hard gate
   `if compaction.degraded { ctx.truncate_to_budget(); }` leaves the entire
   suite passing. It is an **equivalent mutant** within the loop — the applied
   and unpressured outcomes are both under budget by construction, and the
   third decline arm (fewer than `COMPACT_MIN_BLOCKS`) is unreachable from the
   loop, which has pushed a model block and a tool-result block on top of the
   caller's turn before compaction runs. The arm *is* reachable through the
   public `compact_if_pressured` (verified: 6,211 B against a 4,000 B budget,
   `degraded: false`), so the state is pinned at the manager. The existing
   `a_conversation_too_short_to_hold_a_decision_buys_no_compact_call` was
   strengthened with the missing "still over budget **before** the gate"
   assertion — without it its last two lines were satisfied by a context that
   was never over budget. `compact.rs`'s own link-4 row is accurate as written
   and was left alone; the complement is documented in `duty.rs`'s table.
2. **`shell` cannot have a captured-bytes taint proof.** Arming the taint
   backstop requires a configured boundary, and a `shell` result is always
   `ToolProvenance::Unknown`, which the choke point fail-closes on whenever a
   boundary exists — so the *untainted* control sends zero bytes too, and there
   is no non-vacuity pair to be had. Asserted instead by the local answer
   reaching the model's reply, which a resolver test cannot show. Consistent
   with the limitation TASK-061 already disclosed.
3. **`compact`'s content is the turn's context**, so it has no narrower scope to
   demonstrate for BR-7 and a boundary-bearing conversation refuses compaction
   whole. That is BR-7 applied to a duty whose scope is everything, not a gap —
   asserted as its own claim with its own non-vacuity pair.
4. **AC-11's disclosed limitation is now partly closed.** `title` derived its
   ceiling from its contract; `triage` now asserts a **band** (the ceiling must
   also be under 4× the widest legitimate ranking, so widening to 4 KiB goes
   red); `compact` now pins its derivation to `HarnessConfig::default().context_budget_bytes`;
   `shell` gains compile-time pins against the 8 KiB output cap it interprets.
   `shell` is the one that remains only partly closed — "three sentences of
   prose" has no honest byte size to derive from — and that is stated in its
   own test's doc comment rather than left to be rediscovered.

## Technical Notes

Existing harness to reuse, not reinvent:
- `global_capture()` / `assert_no_boundary_bytes()` at `crates/tetond/tests/e2e/harness.rs:47-79`
- `CaptureSse` transport at `crates/tetond/tests/provenance_egress.rs:48-66`
- The taint pattern with its non-vacuity pre-turn at `crates/tetond/tests/e2e/routing_categories.rs:87-215` — note how it asserts the pre-taint turn *genuinely went remote* before asserting the post-taint turn did not. Copy that structure.

**On the mutation table.** Follow `crates/teton/src/loading.rs:30-43`: a
`| Mutation | Fails |` table at the module head listing mutations that were
applied and observed failing. The table is a claim about what the suite actually
catches, so an entry that was reasoned about rather than run makes it a false
claim.

**Run `cargo test --workspace --no-fail-fast`.** Plain `cargo test --workspace`
stops at the first failing target, so a reported failure count from this repo is
a floor, not a total — the workspace has ~34 targets and one red target masks
every target after it.

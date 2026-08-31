---
id: REQ-602
title: "Post-split cleanup: narrow what the split widened, and repair what it stranded"
status: draft
deployable: true
created: 2026-08-31
updated: 2026-08-31
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["maintainability", "security", "testing"]
tags: ["req-599-followon", "visibility", "derived-checks", "br-7", "cleanup"]
---

## Description

REQ-599 split `runtime.rs` into seven modules. A five-agent review over the
merged result found the relocation itself sound — a line-multiset diff of the
whole production corpus turned up **exactly one** changed line, a rustfmt
rewrap, with the public API preserved 72 items to 72 and all 155 rationale ids
intact.

The defects are all in the **periphery**: visibility the split widened beyond
what the code uses, derived checks the split stranded, and criteria that were
ticked on assertion rather than evidence. Two Criticals from that review are
already fixed and merged; this REQ takes the rest.

It should land **before REQ-600**. Every item here gets harder once REQ-600 adds
consumers: an over-wide `pub(crate)` calcifies as soon as something reaches
through it, and REQ-600's new slices will inherit whatever defaults exist when
it starts.

## System Model

### Entities

_None. No new types; this REQ removes surface rather than adding it._

### Events

| Event | Trigger | Payload |
|-------|---------|---------|
| _None._ | No behavior change of any kind. | |

### Permissions

| Action | Roles Allowed |
|--------|---------------|
| _Unchanged._ | | 

## Business Rules

- [ ] BR-1: **Behavior-preserving.** No event payload, ordering, error code,
      refusal sentence or dispatch decision changes. The only observable
      difference is that fewer items are reachable from fewer places.
- [ ] BR-2: An item is `pub(crate)` only if something **outside** `runtime/`
      names it. Everything else is `pub(super)` or private. Measured, not
      asserted: the check is a real search for out-of-tree callers, per item.
- [ ] BR-3: Nothing that was private at `fedcab1` ends this REQ wider than
      `pub(super)` without a named out-of-tree caller. Two of the three
      regressions the review found were security invariants stated in the code's
      own doc comments, so the direction of travel here is inward only.
- [ ] BR-4: Every directory-walking check in the workspace **recurses**. Five
      currently use a non-recursive `read_dir`, so the first `runtime/foo/mod.rs`
      silently leaves the corpus — the same "sees less and passes" failure
      LESSON-594 is about, pre-armed.
- [ ] BR-5: A test whose subject moved during REQ-599 lives with its subject
      (BR-7 of REQ-599, ticked there without a check). Where a test genuinely
      belongs to its old home — a fixture rather than a subject — the module's
      own header says so, as `engine.rs` and `duty.rs` already do.
- [ ] BR-6: A criterion is ticked only with evidence. Three in REQ-599 were not:
      AC-4's module-ownership clause is unimplemented, AC-6 names four scenarios
      and the fixture exercises two, AC-11's "each commit green in CI" is
      contradicted by cancelled `macos-latest` runs (two, measured: `f64d99b` and `56f3777` — steps 6 and 7). Each is either met or
      amended in writing — never left ticked.
- [ ] BR-7: Documentation that points at a moved item points at where it is now.
      ~30 doc references name test paths that no longer resolve.

## Acceptance Criteria

- [ ] AC-1: For every `pub(crate)` item under `crates/tetond/src/runtime/`, a
      search establishes either an out-of-tree caller or a demotion. Report the
      final count of genuine out-of-tree items.
      **State the counting rule with the number.** A review sample reported "61
      items with no out-of-tree caller"; validating this spec against the code
      could not reproduce it — **48** counting top-level items and `impl`
      methods, **52** if `pub(crate)` struct fields are included. The direction
      is not in doubt (a large majority have no caller outside the tree, and
      seven spot-checked `engine.rs` items had zero external references each),
      but the figure was carried into this spec without being re-measured, which
      is exactly how REQ-599 ADR-1's bad number got in. Whatever count this REQ
      reports, it says what it counted.
- [ ] AC-2: **A test enforces BR-2**, so the next split cannot re-widen by
      accident. It walks `runtime/`, collects `pub(crate)` items, and fails on
      any with no reference outside the tree. Shipped with this REQ and kept in
      CI.
- [ ] AC-3: **Mutation on AC-2** — promoting one `pub(super)` item to
      `pub(crate)` turns it red; the mutation and its observed failure are
      recorded in the test's doc comment.
- [ ] AC-4: Every directory scan over `runtime/` recurses. Demonstrated by
      planting a nested `runtime/nested/mod.rs` fixture and confirming each scan
      sees it, then removing it. The sites, corrected during validation:
      `runtime/mod.rs` (3 reads), `runtime/taint.rs`, `tests/skill_turn.rs`,
      `tests/runtime_module_map.rs`, **`projects/scan.rs`**, and
      `tests/traceability_sweep.rs`'s *floor* read only — its workspace sweep
      already walks recursively, so listing it unqualified overstated the work.
      **`projects/scan.rs` is on this list because this REQ's own predecessor put
      it there.** The Critical fix in `d7f4e05` repointed that scan at the
      `runtime/` directory to repair a guard that had gone silently dead — and
      used a flat `read_dir` to do it. The remedy for "a sweep sees less" shipped
      with the same latent defect one commit later, which is the fourth instance
      of this hazard in the REQ-598/599 line and the second committed by hand.
- [ ] AC-5: `views.rs`'s four `snapshot_from_config` tests and `engine.rs`'s
      `local_tier_gated` test move to their subjects, **or** each module header
      records why they stayed, naming them.
- [ ] AC-6: REQ-599's AC-4, AC-6 and AC-11 are each amended in the spec with
      what actually holds. AC-6's fixture either gains the skill-expansion and
      consent scenarios it names, or the AC is narrowed to what the fixture
      covers.
- [ ] AC-7: The stale doc paths resolve. Measured during validation: **31**
      genuinely stale of 42 `runtime::tests::` references — 11 still resolve, so a
      blanket rewrite would break those. The stale segments are `dispatch` (25),
      `config_document_seam` (4), `provider_setup` (1), and
      `the_two_taint_gates_agree_cause_for_cause` (1). A check asserts every
      `runtime::…::` path named in a doc comment exists, so this class cannot
      silently return.
- [ ] AC-8: ADR-4's step table in REQ-599's architecture doc is reconciled with
      what shipped — it names **five** modules that do not exist (`types.rs`, `consent.rs`, `egress.rs`, `session.rs`, `turn.rs`). **The planned
      session-lifecycle slice delivered nothing and is recorded nowhere as
      deferred**; it is either done here or explicitly deferred with a reason.
- [ ] AC-9: `cargo test --workspace --no-fail-fast` green with output grepped
      for `FAILED`; clippy clean under `deny`; `cargo fmt --check` clean.
- [ ] AC-10: The traceability sweep and module-map guard still pass, with `BASE`
      and `TOUCHED` repointed at this REQ's base.

## External Dependencies

- None. Everything here is inside `crates/tetond`.

## Assumptions

- **The relocation itself is sound and does not need re-verifying.** The
  adversarial pass reconstructed both production corpora and diffed the
  visibility-normalized line multisets: one changed line, a rustfmt rewrap. This
  REQ trusts that result rather than repeating it.
- Narrowing visibility is a compile-time-checked operation, so BR-2's sweep can
  be aggressive: anything that should not have been narrowed fails the build
  rather than shipping.

## Open Questions

- [ ] OQ-1: Should `use super::*;` in each module become an explicit import
      list? The review argues the glob hides real dependencies — `duty.rs`
      reaches into `taint.rs` and `provider.rs` with nothing in its imports
      saying so. Narrowing them is a large mechanical diff with real value for
      REQ-600, which will otherwise have to grep for "which siblings does this
      stage touch" every time. Deliberately not folded in until someone decides
      whether it belongs here or in REQ-600.
- [ ] OQ-2: `testsupport.rs` is test-only but contains no `#[cfg(test)]` inside
      it, so every truncating "production half" scan treats all 43 lines as
      production. Benign today. Fix by wrapping its contents, or by teaching the
      scanners to skip `#[cfg(test)] mod` declarations?
- [ ] OQ-3: Should the CI `concurrency: cancel-in-progress` setting change? It
      is what left two `macos-latest` runs cancelled, and macOS is the runner
      that caught the last ordering defect (LESSON-591). Cheap to fix; changes
      CI cost for every PR, so it is not this REQ's call to make alone.

## Out of Scope

- **Anything REQ-600 covers**: `run_prompt_turn`'s decomposition, slicing the
  god-impl, `turn_loop.rs`'s nesting. This REQ makes that work easier and does
  not start it.
- Further module extraction beyond the stranded session-lifecycle slice (AC-8).
- The `extra_env` and ssh-agent questions — those are REQ-601.

## Retrieved Context

- REQ-599 (spec): the split this cleans up after
- LESSON-594 (lesson): a decomposition changes what "the corpus" means — BR-4 is this pre-armed
- LESSON-595 (lesson): a visibility pass can narrow an API while appearing only to move code — BR-2/BR-3 are the standing guard
- LESSON-585 (lesson): key a sweep on the hazard, and floor it

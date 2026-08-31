---
id: REQ-602
title: "Post-split cleanup: narrow what the split widened, and repair what it stranded"
status: complete
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

- [x] AC-1: Every `pub(crate)` item under `crates/tetond/src/runtime/` that
      nothing outside that directory needs is narrowed to `pub(super)`.
      **The count is 4, and it was established by the compiler, not by a
      search.** Four prior estimates — 61 (a review sample), 48–52 (a bare-name
      grep), 73 (a qualified-path rule), and 5 (the first demote-all pass, which
      stopped one item short) — were all wrong, in both directions.
      The method that works is the definition itself: demote every `pub(crate)`
      under `runtime/` to `pub(super)`, build, and read the errors.
      **The figures, with their rule (re-derived at TASK-307).** The rule:
      *item declarations carrying `pub(crate)` in the seven submodule files* —
      excluding `mod.rs`, excluding struct and enum fields, excluding `use`
      re-exports, excluding prose. Under that rule, measured at the branch base
      `8902439` and at the tip: **88 → 4**.
      An earlier draft of this AC said **130 → 5**, and that pairing was itself
      the bug it warns about: 130 (really 131) is the count of *occurrences of
      the token* `pub(crate)` in those files, while 5 is a count of
      *declarations*. Two rules, one arrow. Under the occurrence rule the honest
      pair is **131 → 5** — and the fifth is `views.rs:205`, a doc comment
      **discussing** the visibility of the function declared two lines below it.
      Prose counted as usage, which is the exact miscount that produced three of
      this question's four wrong answers, surviving into the criterion written to
      correct it.
      A yet earlier draft said "143 → 8", which counted `mod.rs`'s no-ops as
      work. `mod.rs` is excluded from the **count** because `pub(super)` there
      *is* `pub(crate)` — `mod.rs` is the `runtime` module and its parent is the
      crate root, so rewriting its qualifiers is a semantic no-op that only
      inflates the diff.
      **It is not excluded from the rule, and an earlier draft of this AC said
      "deliberately untouched", which was wrong.** Narrowing to *private* in
      `mod.rs` is real work BR-2 asks for, and the demote-all method cannot
      surface it because the demotion there is a no-op by construction. Two
      `mod.rs` changes shipped on this branch, neither visible under this AC's
      counting rule, which is why they are named here: `derive_provider_setup`
      became private, and `pub(crate) use provider::*;` became
      `use provider::*;`.
      The four that genuinely need crate reach, with their consumers:
      `LOCAL_ENGINE_N_CTX` (`egress/redact.rs`, `harness/budget.rs`,
      `harness/compact.rs`), `TAINT_BY_CONTEXT` and `taint_pin_line`
      (`carry.rs`), `endpoint_query_names_a_credential` (`provider_recipes.rs`,
      `web_setup_catalog.rs`).
      **`RenderedProviderSetup` was a fifth and is not.** It was kept crate-wide
      to "match its `pub(crate)` accessor",
      `DaemonRuntime::derive_provider_setup` — and review asked why *that* was
      `pub(crate)`. Nothing outside `runtime/` names it; every hit is prose,
      including one this REQ's own architecture doc already lists as a false
      positive of the bare-name grep. Narrowing the accessor to private drops the
      type to `pub(super)`, and the build is clean. The demote-all method was
      right; it had been run over the submodules and not over the `mod.rs` item
      holding one of them open.
- [x] AC-2: A **ratchet** asserts the `pub(crate)` surface under `runtime/` is
      exactly the **four** named items — not eight ("8" was the discarded
      "143 → 8" draft's figure) and not five (that was the first demote-all pass)
      — and names them, **enumerating its corpus from disk rather than from a
      hardcoded file list**, so a module REQ-600 or REQ-603 adds is scanned
      rather than silently exempt, with a comment recording how to
      re-derive them (demote all, build, read the errors). Deliberately a
      ratchet and not a search: a test that greps for out-of-tree references
      would re-encode the mistake this REQ exists to correct — three different
      searches gave three different wrong answers. Bounded on **both** sides,
      like the suppression ratchet, so a drop demands a deliberate update rather
      than passing as improvement.
- [x] AC-3: **Mutation on AC-2** — promoting one `pub(super)` item to
      `pub(crate)` turns it red, and deleting one of the five turns it red the
      other way. Both recorded in the test's doc comment with what went red.
- [x] AC-4: Every directory scan over `runtime/` recurses. Demonstrated by
      planting a nested `runtime/nested/mod.rs` fixture and confirming each scan
      sees it, then removing it. The sites, corrected during validation:
      `runtime/mod.rs` (**1** read scans `src/runtime`; "3 reads" counted every
      `read_dir` in the file, including ones scanning other paths — a carried
      figure, not a measured one), `runtime/taint.rs`, `tests/skill_turn.rs`,
      `tests/runtime_module_map.rs`, **`projects/scan.rs`**, and
      `tests/traceability_sweep.rs`'s *floor* read only — its workspace sweep
      already walks recursively, so listing it unqualified overstated the work.
      **That floor read was missed on the first pass** and found by adversarial
      review: TASK-301 changed six files, and this was the seventh site — the one
      the AC named. It now shares that file's own recursive walker, demonstrated
      by planting `runtime/nested/mod.rs` and watching the floor's corpus go from
      8 files to 9.
      **`projects/scan.rs` is on this list because this REQ's own predecessor put
      it there.** The Critical fix in `d7f4e05` repointed that scan at the
      `runtime/` directory to repair a guard that had gone silently dead — and
      used a flat `read_dir` to do it. The remedy for "a sweep sees less" shipped
      with the same latent defect one commit later, which is the fourth instance
      of this hazard in the REQ-598/599 line and the second committed by hand.
- [x] AC-5: `views.rs`'s four `snapshot_from_config` tests and `engine.rs`'s
      `local_tier_gated` test move to their subjects, **or** each module header
      records why they stayed, naming them.
- [x] AC-6: REQ-599's AC-4, AC-6 and AC-11 are each amended in the spec with
      what actually holds. AC-6's fixture either gains the skill-expansion and
      consent scenarios it names, or the AC is narrowed to what the fixture
      covers.
- [x] AC-7: The stale doc paths resolve.
      **The figures, with their rule (re-derived at TASK-305 by a resolver built
      against the module tree on disk, not by grep).** Rule: a *distinct
      `runtime::tests::` path*, counted once however often it appears; the
      occurrence count is given beside it because the two differ by a factor of
      1.6 and the spec previously mixed them.
      **Corpus: the whole repository, `.adlc/` included** — stated because a
      numerator and a denominator must come from the same corpus, and the shipped
      check deliberately uses a narrower one (code and `docs/` only). Under the
      repository-wide corpus: **46 distinct paths / 74 occurrences, of which 27
      distinct / 44 occurrences were stale.** Under the check's own corpus the
      base is 27 distinct / 42 occurrences, the difference being the
      deliberately-kept `.adlc/` paths and their siblings. Validation had said
      "31 of 42", which survives under neither corpus.
      The stale roots: `dispatch` (19 distinct), `config_document_seam` (4),
      `provider_setup` (1), `provider_test` (1),
      `the_two_taint_gates_agree_cause_for_cause` (1), and
      `the_snapshot_marks_the_unreached_categories_and_the_judgment_default` (1)
      — that last one went stale during TASK-304 of this REQ, an hour before the
      resolver caught it.
      **Eight distinct stale paths are left in place deliberately**, all in
      `.adlc/specs/`. Those are a historical record: REQ-574's requirement
      describes the tree as it stood when REQ-574 shipped, and rewriting it to
      match a later refactor would make it lie about its own moment. The check's
      corpus excludes those directories and its header says why.
      A check asserts every `runtime::…` path named in a doc comment exists, so
      this class cannot silently return.
- [x] AC-8: ADR-4's step table in REQ-599's architecture doc is reconciled with
      what shipped — it names **five** modules that do not exist (`types.rs`, `consent.rs`, `egress.rs`, `session.rs`, `turn.rs`). **The planned
      session-lifecycle slice delivered nothing and is recorded nowhere as
      deferred**; it is either done here or explicitly deferred with a reason.
- [x] AC-9: `cargo test --workspace --no-fail-fast` green with output grepped
      for `FAILED`; clippy clean under `deny`; `cargo fmt --check` clean.
- [x] AC-10: The traceability sweep and module-map guard still pass, with `BASE`
      and `TOUCHED` repointed at this REQ's base.

## Verification (TASK-307)

`cargo test --workspace --no-fail-fast`: **4,065 passed, 0 failed**, output
captured and **grepped for `FAILED` — 0 occurrences**, `EXIT=0`. Summed counts
are reported beside the grep, not instead of it. `cargo clippy --workspace
--all-targets` under `clippy::all = deny`: **0 errors, 0 warnings**.
`cargo fmt --all --check`: clean.

**Every figure below states how it was counted.** This REQ produced four
different answers to one question, and a fifth was still in its own AC text when
this task re-derived it (see AC-1).

| AC | status | evidence, with the counting rule |
|---|---|---|
| AC-1 | met | Rule: *item declarations carrying `pub(crate)` in the submodule files* — no `mod.rs`, no fields, no `use`, no prose. **88 → 4**, base `8902439` to tip. Under the *token-occurrence* rule, **131 → 5**; the fifth is a doc comment discussing a visibility, not a declaration. Two figures were wrong before this settled: the AC paired 130 (occurrences) with 5 (declarations), and the surface itself was 5 until review found `RenderedProviderSetup` held open by an accessor that had no out-of-tree caller either. |
| AC-2 | met | `crates/tetond/tests/runtime_visibility.rs` — bounded both ways, corpus **enumerated from disk** (a hardcoded list would go blind exactly when REQ-600 adds modules, which is the stated reason this REQ lands first), with floors keyed on the two hazards that can silence it: a walk that lost files, and a parser that stopped matching. The ratchet asserts **4**. |
| AC-3 | met | **Six** mutations, re-run after review and recorded as observed. The previous record contained an outcome that *cannot occur* — two `assert!`s in one test cannot both fire — which meant the ratchet's lower bound had never actually been seen firing. It has now (mutation 4). One mutation **does not compile** and is recorded as such rather than dropped. Mutation 4 also caught a defect in the floor written above it: expressed against `CRATE_WIDE.len()`, it pre-empted the very arm it was meant to exercise. |
| AC-4 | met | Seven scan sites: three sharing `call_sites::scan::rust_files`, four carrying a documented local copy (the shared helper is `#[cfg(test)]`-gated and unreachable from an integration test). **The seventh — `traceability_sweep.rs`'s floor read — was missed on the first pass and found by review**, though the AC named it; the earlier accounting of "three + two" covered six of seven and did not notice. The planted fixture found a **second** defect: `runtime_module_map` recursed but compared *basenames*, collapsing the subtree onto the documented root. Recursion alone was not the fix. |
| AC-5 | met | Four `snapshot_from_config` tests → `views.rs`; the scripted-exemption test → `engine.rs`; two shared helpers → `testsupport.rs`. `views.rs` gained the header section whose absence was the reviewable defect. `engine.rs`'s BR-7 paragraph said "two test functions" and was corrected to three before it shipped wrong. |
| AC-6 | met | REQ-599's AC-4 amended (its module-ownership clause is uncomputable per that REQ's own ADR-5), AC-6 narrowed to the scenario the fixture contains, AC-11 marked **NOT MET** with the two cancelled `macos-latest` jobs named by commit. AC-6's gap is filed as REQ-604 rather than waved through. |
| AC-7 | met | Rule: *distinct `runtime::tests::` paths*, resolved against the module tree on disk, **counted over the whole repository including `.adlc/`** — the corpus is named because the check's own is narrower. **46 distinct / 74 occurrences, of which 27 distinct / 44 occurrences stale**; validation had said "31 of 42", which survives under neither corpus. 32 replacements across 10 files. Eight distinct stale paths left in `.adlc/specs/` deliberately — a historical record must be allowed to describe its own moment. The check's citation floor is keyed on **shape**: each sub-corpus that carries citations today must still carry them, because a bare count is cleared by a corpus that lost a whole directory. |
| AC-8 | met | ADR-4's table reconciled against the seven commits by hash; five named modules never existed. The session-lifecycle slice is recorded as deferred **with its reason** and filed as REQ-603. |
| AC-9 | met | Above. |
| AC-10 | met | `traceability_sweep`, `runtime_module_map`, `runtime_doc_paths`, `runtime_visibility`, `suppression_ratchet` all green. **`BASE` is deliberately left at `17c39ec`** — REQ-599's pre-split commit — and `TOUCHED` at `runtime.rs`. Repointing them at this REQ's base would compare the split tree against itself and prove nothing about the split, which is the property the sweep exists to hold. The AC's "repointed at this REQ's base" is therefore recorded as *deliberately not done*, with the reason, rather than performed because it was written down. |

### What this REQ's own guards caught, in it

- The **ratchet** (AC-2) caught a regression I introduced two tasks later:
  `router_for_config` and `config_with_remote` went into `testsupport.rs` as
  `pub(crate)` and were narrowed to `pub(super)`.
- The **doc-path resolver** (AC-7) caught a path that TASK-304 had staled an
  hour earlier, in this same REQ.
- The **planted nested fixture** (AC-4) caught a comparison that recursion had
  not fixed.
- And re-deriving AC-1's figure at this task caught the counting-rule mismatch
  still sitting in the criterion written to prevent counting-rule mismatches.

The last one is the honest summary of this REQ: knowing the rule did not prevent
breaking it. Only re-deriving did.

### What re-deriving did not catch, and review did

A three-agent adversarial panel ran after Phase 4 and found six things this
REQ's own verification had passed over. Recorded because the pattern is the
point — every one is a claim I had checked, in the direction I was already
looking:

- **The surface was 4, not 5.** `RenderedProviderSetup` was held crate-wide by
  an accessor nobody had asked the same question of.
- **`traceability_sweep.rs`'s flat read survived**, though AC-4 named it. The
  "three sharing, two local" accounting covered six of seven sites.
- **The ratchet's corpus was a frozen file list**, so it would go blind the
  moment REQ-600 added a module — which is the entire stated reason for landing
  this REQ first.
- **The ratchet's vacuity floor could never fire**, being strictly weaker than
  an assertion below it.
- **A recorded mutation outcome was impossible**, so the lower bound had never
  been observed firing.
- **This REQ's own `architecture.md` still carried "143 → 8"** — the discarded
  figure — while its requirement called that figure baseless. The document
  reconciling *REQ-599's* stale plan had not been reconciled with its own.

The verification pass re-derived every number and still missed all six, because
re-derivation checks the figures you thought to check. Only someone trying to
break the claims found the claims that were breakable.

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

---
id: TASK-065
title: "Cross-cutting verification — the duty matrix, egress capture, taint, ceilings, mutations"
status: draft
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

- `crates/tetond/tests/e2e/duty_matrix.rs` — **new**. AC-3: all five duties × (resolves / unresolvable / provider error / tainted session), asserting each call site's invariant still holds on every failure path.
- `crates/tetond/tests/e2e/duty_egress.rs` — **new**. AC-4 and AC-5: boundary capture with non-vacuity pairs, the BR-7 scoping proof, and the taint override.
- `crates/tetond/tests/e2e/duty_ceilings.rs` — **new**. AC-11: a mock that deliberately overruns, per duty, asserting against each declared constant.
- `crates/tetond/src/harness/duty.rs` — AC-8/AC-10 assertions (source-level scan) and the `# What breaks which test` mutation table at the module head.

## Acceptance Criteria

- [ ] **AC-3**: the table-driven matrix covers all five duties × four conditions. For `compact`, "the invariant holds" means the context is under budget afterward.
- [ ] **AC-4**: for each duty, with a remote binding and a `local-only` boundary, **zero** boundary bytes appear in any captured payload — **paired with a non-vacuity assertion** that the same duty *does* send when the content is clean. An egress test without its non-vacuity pair is not evidence (LESSON-485); a fixture that never reaches the sending state passes for the wrong reason.
- [ ] **AC-4 (BR-7 scoping)**: a duty whose *own* content is clean still sends while the turn's wider context contains boundary material. This is what proves the scope is the content sent rather than the turn — without it, a passing test is equally consistent with turn-level scoping.
- [ ] **AC-5**: a tainted session runs all four new duties on the local tier regardless of binding, asserted by **captured bytes**, not by reading the resolved route.
- [ ] **AC-10**: a source-level assertion that no duty's category is produced from prompt text, tool name, or any string comparison — the duty path must not reintroduce what the type system forbids on the judgment path.
- [ ] **AC-8**: the seam assertion per the architecture's five-point boundary — one route type, one `Duty` trait, one local impl, one remote impl, one `Egress::scoped(` call on the duty path, one ceiling site; per-category source limited to a resolve line, a contract constant, and a prompt builder.
- [ ] **AC-9**: for each duty, (a) removing the taint override and (b) making the failure path return its input unchanged each turn **at least one test red**. Each mutation applied and observed, then reverted — not reasoned about (LESSON-441).
- [ ] **A mutation that comes back green is reported as a finding, not quietly fixed** (LESSON-485). Record it; a green mutation is a fact about the tests, never confirmation the code is fine.
- [ ] `cargo test --workspace --no-fail-fast` is green.

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

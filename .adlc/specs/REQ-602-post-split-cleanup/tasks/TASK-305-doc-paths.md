---
id: TASK-305
title: "Repoint the 31 stale doc paths, and keep them pointed"
status: complete
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: [TASK-301]
---

## Description

AC-7. 31 of 42 `runtime::tests::` references no longer resolve. **11 still do** — a
blanket rewrite breaks those, which is why the count matters more than the
pattern.

Stale segments: `dispatch` (25) → `runtime::duty::dispatch`,
`config_document_seam` (4) → `runtime::config_document::tests`,
`provider_setup` (1), `the_two_taint_gates_agree_cause_for_cause` (1).

## Files to Create/Modify

- `crates/tetond/src/harness/duty.rs`, `harness/compact.rs`, several
  `crates/tetond/tests/*.rs`, `crates/teton/tests/*.rs`, `docs/manual-verification.md`

## Acceptance Criteria

- [ ] All 31 stale paths resolve; the 11 live ones are untouched.
- [ ] A check asserts every `runtime::…::` path named in a doc comment exists,
      using TASK-301's recursive walker, so the class cannot silently return.
- [ ] The check has a vacuity floor — a parser matching nothing agrees with any
      tree (LESSON-585).
- [ ] Mutation: break one path, confirm the check goes red; record it.

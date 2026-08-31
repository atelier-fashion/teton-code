---
id: TASK-302
title: "Narrow every pub(crate) under runtime/ that nothing outside needs"
status: complete
parent: REQ-602
created: 2026-08-31
updated: 2026-08-31
dependencies: []
---

## Description

143 `pub(crate)` sites → 8. The method is ADR-1's: demote all to `pub(super)`,
build, and let the errors name the survivors. Not a search — three searches gave
three different wrong answers, the third from someone who had just corrected the
second.

## Files to Create/Modify

- All eight files under `crates/tetond/src/runtime/`

## Acceptance Criteria

- [ ] Exactly five item declarations remain `pub(crate)`: `LOCAL_ENGINE_N_CTX`,
      `TAINT_BY_CONTEXT`, `taint_pin_line`,
      `endpoint_query_names_a_credential`, `RenderedProviderSetup`.
- [ ] The three glob re-exports (`engine`, `taint`, `views`) stay
      `pub(crate) use` and now carry only those crate-wide members — verified by
      the absence of the "glob doesn't reexport anything" clippy warning.
- [ ] **Nothing is widened.** No item ends this task more visible than at
      `fedcab1`, and the five `pub use` items REQ-599 named stay `pub`. This
      direction has broken twice (LESSON-595), both times in a bulk pass.
- [ ] `pub(super)` in `mod.rs` is left alone where it is already correct — it
      *is* `pub(crate)` there, so changing it is a no-op that only inflates the
      diff.
- [ ] `cargo check --workspace --all-targets` clean; clippy clean under `deny`;
      `cargo fmt --check` clean.

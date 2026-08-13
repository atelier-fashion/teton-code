---
id: TASK-126
title: "Assert every ConfigError variant Config::validate can raise"
status: complete
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: []
---

## Description

Implement BR-10. `Config::validate()` is fail-closed and gates daemon startup,
so an unasserted variant is an unguarded startup gate. Four variants have zero
test references today. Fully independent of the provenance work — start it in
parallel.

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — add the four missing assertions and the enumeration check to the existing inline `#[cfg(test)]` module.

## Acceptance Criteria

- [x] AC-10: `UnknownDefaultProvider`, `UnknownCategoryProvider`, `UnknownTierFallback`, and `WebPermissionAllowNamesOff` each have a test asserting they are raised for their triggering input.
- [x] AC-11: a check enumerates every `ConfigError` variant constructed in `Config::validate()` and fails if any lacks an asserting test — so BR-10 holds for variants added later, not just today's four.
- [x] Tests live INLINE in `crates/teton-core/src/config.rs`, matching the crate's existing convention. `teton-core` has no `tests/` directory and must not gain one for this.
- [x] New assertions match the existing shape: `assert_eq!(cfg.validate().unwrap_err(), ConfigError::Variant { .. })`.
- [x] `cargo clippy --all-targets` clean.

## Technical Notes

Verified absent at spec time: each of the four has zero references past the
`#[cfg(test)]` boundary at `config.rs:2002`, while every named sibling variant
has at least one. Existing pattern to copy: `config.rs:2637`.

Raise sites: `UnknownDefaultProvider` 1195, `UnknownCategoryProvider` 1313,
`UnknownTierFallback` 1297, `WebPermissionAllowNamesOff` 1749.

For AC-11, prefer a source-scanning test over a hand-maintained list — the
hand-maintained list is the failure mode being fixed. Note that
`crates/tetond/tests/` already has precedent for source-scanning tests; see
BUG-159 for the trap where such a test panics when `src` changes mid-run.

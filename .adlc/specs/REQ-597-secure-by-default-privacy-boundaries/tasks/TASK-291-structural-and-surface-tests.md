---
id: TASK-291
title: "Structural guard, reporting surface, and the upgrade check"
status: pending
parent: REQ-597
repo: teton-code
created: 2026-08-29
updated: 2026-08-29
dependencies: [TASK-287, TASK-288, TASK-289]
---

## Description

The tests that keep the design from drifting after it ships. Covers AC-8, AC-9, AC-9.1 (the
end-to-end half), and AC-10.

## Files to Create/Modify

- `crates/tetond/tests/boundary_coverage.rs` — the AC-8 region check, following the file's
  existing `include_str!`-at-compile-time technique (BUG-159: a runtime read scans the wrong
  tree).
- `crates/teton/src/main.rs` tests (or the CLI's existing surface-test module) — AC-9.
- `crates/tetond/tests/config_preservation.rs` — AC-10.

## Acceptance Criteria

- [ ] AC-8: a source-level **region** check asserts `DEFAULT_BOUNDARIES` is referenced in
      exactly one composition site. A second composition site fails the test. The check is a
      region check, not a count of call sites — relocating a call keeps a count identical
      (conventions.md's sweep-not-count rule, LESSON-568).
- [ ] AC-9: `boundary list` output includes every builtin row alongside the user's, each
      labelled with its origin, asserted by inspecting the **rendered lines** — not an exit
      code (LESSON-519).
- [ ] AC-9: a test pins that `teton boundary list` and `/boundary list` still share one body,
      so a future divergence cannot leave one surface reporting a set the other does not.
- [ ] AC-9.1 (end to end): a `config/get` snapshot crossing the real wire preserves each row's
      origin; a snapshot omitting the field is read as `User`.
- [ ] AC-10: a config that already declares `[[boundaries]]` gains the builtin set on upgrade
      **without a config rewrite**, and its own rows are **byte-unchanged on disk**. Assert the
      file's bytes before and after, not a parsed round trip — the failure this guards against
      is `origin = "user"` appearing in the user's file, which a parsed comparison would not see.
- [ ] Each test records its mutation in its doc comment, per conventions.md.

## Technical Notes

AC-10 is the one with a real trap behind it. The config writer (`config_doc.rs::apply_config_delta`)
diffs `Config` against `canonical_document(config)` and edits the user's TOML surgically. Two
things must hold for the bytes to be unchanged, and the test should be able to fail if either
breaks: builtins never enter `Config.boundaries` (ADR-1), and `origin` skips serialization for
`User` rows (ADR-3). Drive the assertion through an **unrelated** `config/set` — a provider add,
say — so the test proves the boundaries table survives a write it did not initiate. A test that
only reads the config back proves nothing about the writer.

For AC-8, `boundary_coverage.rs` already embeds `../../teton-core/src/boundary.rs` via
`include_str!`; add `config.rs` the same way and scan its production half (the file's
`production_half` helper strips `#[cfg(test)]`).

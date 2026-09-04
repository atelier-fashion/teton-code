---
id: TASK-379
title: "`[context] generate = ask|always|never` and its durable `ConfigUpdate`"
status: complete
parent: REQ-613
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

The durable switch (spec System Model, ADR-2's short-circuits read it): a `GenerateMode` enum on
REQ-612's `ContextConfig`, default `ask`, rendered only when the user named the table, and the
`ConfigUpdate::SetRepoContextGenerate { mode }` struct variant that `teton context generate`
writes through `config/set`. Requires REQ-612 TASK-369 merged (the table exists).

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `GenerateMode { Ask, Always, Never }` (`snake_case`),
  `ContextConfig.generate: GenerateMode` with `#[serde(default)]`, `Default` = `Ask`.
- `crates/teton-protocol/src/methods.rs` — `ConfigUpdate::SetRepoContextGenerate { mode }` (struct
  variant, the `methods.rs:2091` reason); the shape test gains the row.
- `crates/tetond/src/runtime/mod.rs` — persistence through the same write path as
  `SetTranscriptEnabled`; `config/get` shows the posture.
- `crates/tetond/src/runtime/config_document.rs` — render `generate` inside `[context]` only when
  the user named the table.
- `crates/tetond/tests/config_preservation.rs` — round-trip and the refuse/accept pair.

## Acceptance Criteria

- [x] A config with no `[context]` table parses to `generate == Ask`; `generate = "never"` parses;
      an unknown value is a structural error naming the three values.
- [x] `SetRepoContextGenerate` writes the key and re-parses to the same value; a refused write
      leaves the bytes identical — the LESSON-519/520 pair.
- [x] `cargo test -p teton-core --no-fail-fast` and
      `cargo test -p tetond --test config_preservation --no-fail-fast` green.
- [x] LESSON-587 audit recorded on `ContextConfig::is_unset`: it is the only predicate in the
      tree that branches on a `[context]` field holding its default, and adding a field to a
      whole-struct equality can only move configs from "unset" to "set" — no dormant rule is
      switched on. No `is_empty`-style predicate was added.

## Delivery notes

**Scope split.** `ConfigUpdate::SetRepoContextGenerate`, the `RepoContextGenerateMode` wire twin
and the daemon's persistence/`config/get` arms were delivered by TASK-380 (commit `7b3db53`);
this task delivers the `teton-core` schema, the rendering finding, and the three tests.

**No rendering code was needed in `config_document.rs`.** REQ-612's
`skip_serializing_if = "ContextConfig::is_unset"` on `Config::context` plus REQ-574's
`diff(current, candidate)` delta rule already give "rendered inside `[context]` only when the
write is about it, and never invented in a table the user did not name". Recorded as a test
rather than a claim, since it is a property of attributes in another crate and a diff in a third.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-10 | test-case | `crates/teton-core/src/config.rs::context_generate_defaults_to_ask_and_names_its_three_values` | yes |
| AC-11 | test-case | `crates/tetond/tests/config_preservation.rs::set_repo_context_generate_writes_the_key_and_a_refused_write_leaves_the_bytes_identical` | yes |

## Technical Notes

`never` suppresses the offer only; `/context init` ignores it (BR-8). Do not add an
`is_empty`-style predicate anywhere — LESSON-587's audit for a non-empty default.

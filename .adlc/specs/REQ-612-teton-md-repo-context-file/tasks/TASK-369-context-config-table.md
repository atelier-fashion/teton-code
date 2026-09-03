---
id: TASK-369
title: "The [context] table: `repo_file`, default true, rendered only when named"
status: complete
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

The durable half of BR-2 with no behaviour change yet: a `[context]` table on `Config` holding
`repo_file: bool` (default `true`), the `config_document` rule that the table is written only
when the user named it, and structural validation (there is nothing to validate beyond parse).
Landing it alone pins the default before anything reads it (ADR-6).

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `ContextConfig { repo_file: bool }` with
  `#[serde(default)]` on the struct and the field, `Default` = `true`; `Config.context:
  ContextConfig` with `#[serde(default)]`. Follow `TranscriptConfig` (line 943) for shape and
  `PrivacyConfig` for "states its posture on write".
- `crates/tetond/src/runtime/config_document.rs` — render `[context]` on write only when the
  source document named it; a config that never named the table does not gain it on an
  unrelated write.
- `crates/tetond/tests/config_preservation.rs` — round-trip cases.

## Acceptance Criteria

- [x] BR-2: a `Config` parsed from TOML with no `[context]` table has `context.repo_file == true`;
      `[context]` with `repo_file = false` parses to `false`; an unrelated write preserves both.
- [x] LESSON-587 check recorded in the PR: no predicate in the tree branches on the emptiness of
      anything the new table introduces (grep `context.` readers; none exist yet).
- [x] `cargo test -p teton-core --no-fail-fast` and the preservation suite are green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-2 | test-case | `crates/teton-core/src/config.rs::context_table_defaults_repo_file_to_true` | yes |
| BR-2 | test-case | `crates/tetond/tests/config_preservation.rs::a_named_context_table_survives_an_unrelated_write_and_an_unnamed_one_is_not_added` | yes |

## Technical Notes

Default `true` is the feature (a `CLAUDE.md`-class file is expected to work on install). The
session switch is deliberately **not** a field here — the `/transcript` split (`config.rs:935–940`).

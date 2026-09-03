---
id: TASK-360
title: "The [transcript] table and the data-dir resolver"
status: draft
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

The config half of the REQ, with no behaviour change yet: a `[transcript]` table on `Config`,
a `resolve_data_dir` beside `resolve_base_dir`, and the pure `effective_dir` derivation (ADR-4).
Covers BR-1's "off by default" posture, BR-13's keys, and AC-19. Nothing reads the table yet;
landing it alone pins the semantics before the sink depends on them. `effective_boundaries()`
is **not** touched — validation refuted the boundary-row design (architecture ADR-7); the
transcript directory is denied in the tool jail by TASK-368 instead.

## Files to Create/Modify

- `crates/teton-core/src/config.rs` — `TranscriptConfig { enabled: bool, dir: Option<PathBuf>,
  retain_days: u32, max_record_bytes: usize }` with `#[serde(default)]` on the struct and on
  each field, `enabled` serialized unconditionally within the table (the `PrivacyConfig`
  pattern, lines 102–141); defaults `false`, `None`, `30`, `65536`. `Config.transcript:
  TranscriptConfig` with `#[serde(default)]`.
  `TranscriptConfig::effective_dir(&self, data_dir: &Path) -> PathBuf`. `Config::validate()`
  gains structural checks only: `max_record_bytes >= 1024`, `dir` absolute when set.
- `crates/teton-core/src/config_doc.rs` — the `[transcript]` table renders on write with the
  same "states its posture" rule as `[privacy]`; a config that never named `[transcript]` does
  not gain the table on an unrelated write.
- `crates/teton-protocol/src/socket_path.rs` — `resolve_data_dir(xdg_data_home: Option<PathBuf>,
  home: Option<PathBuf>) -> PathBuf` and a `data_dir()` composition beside `daemon_paths()`.

## Acceptance Criteria

- [ ] BR-1: a `Config` parsed from a TOML with no `[transcript]` table has `transcript.enabled ==
      false`; one with `[transcript]` and no `enabled` key also parses to `false`.
- [ ] BR-13: `retain_days` defaults to `30` and `max_record_bytes` to `65536`; `retain_days = 0`
      parses and validates.
- [ ] `effective_dir`: `dir` set → that path; unset → `<data_dir>/transcripts`. Pure, no I/O.
- [ ] AC-19 (unit half): rendering a `Config` through `config_doc` whose source TOML never named
      `[transcript]` emits no `[transcript]` table; one whose source did name it re-emits it with
      the user's keys only — the effective directory never appears unless `dir` was written
      (assert on the rendered text).
- [ ] `effective_boundaries()` is byte-identical to `main` — REQ-597's region test and its index
      assertions pass unchanged.
- [ ] Validation: `max_record_bytes = 10` and a relative `dir` are structural errors; a valid
      table passes. Config validity vs usability per `conventions.md` — no other refusals at load.
- [ ] `resolve_data_dir`: `xdg_data_home` wins when set; macOS home form otherwise; temp dir when
      neither. A table test mirrors the existing `resolve_base_dir` tests.
- [ ] `cargo test -p teton-core -p teton-protocol --no-fail-fast` is green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/teton-core/src/config.rs::transcript_table_defaults_to_off_and_states_its_posture` | no |
| BR-13 | test-case | `crates/teton-core/src/config.rs::transcript_retention_and_record_size_defaults` | no |
| AC-19 | test-case | `crates/teton-core/src/config_doc.rs::the_transcript_table_is_written_only_when_the_user_named_it` | yes |

## Technical Notes

`effective_dir` is pure and lives on `TranscriptConfig` so the CLI can render it in `doctor`
without a daemon round-trip for the default case.

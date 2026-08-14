---
id: TASK-135
title: "Pure config-document delta engine in teton-core"
status: draft
parent: REQ-574
created: 2026-08-14
updated: 2026-08-14
dependencies: []
repo: teton-code
---

## Description

Create the format-preserving delta engine: a pure module that takes the
on-disk TOML text plus the caller's pre-mutation `current` and `candidate`
`Config`s, and returns the edited document text — comments, key order,
whitespace, and unknown keys untouched everywhere except the keys whose
canonical serialization differs between `current` and `candidate` (ADR-1).
Also the section-extraction helper the preview will use (ADR-2).

## Files to Create/Modify

- `crates/teton-core/src/config_doc.rs` — NEW module: `apply_config_delta(doc_text: &str, current: &Config, candidate: &Config) -> Result<String, DeltaError>`; `table_section(doc_text: &str, key: &str) -> Option<String>`; `DeltaError` (thiserror) with a `Parse` variant carrying the toml_edit error display
- `crates/teton-core/src/lib.rs` — export the new module
- `crates/teton-core/Cargo.toml` — add `toml_edit = "0.22"` (already in Cargo.lock transitively via toml 0.8)

## Acceptance Criteria

- [ ] `apply_config_delta` computes the delta by diffing canonical serializations of `current` vs `candidate` (recursive over tables; arrays — value arrays AND arrays-of-tables — are one key, replaced wholesale when different; keys in current-canonical but absent from candidate-canonical are removed from the document)
- [ ] A fixture embedding the README's commented `[web]` block **verbatim** (incl. `search_auth = "X-Subscription-Token: {key}"`), plus an unknown key inside `[web]` and an unknown top-level table, survives a tier-only delta byte-for-byte outside the changed key (LESSON-512, spec AC-1 groundwork)
- [ ] Removal semantics: removing a key removes its attached comment decor; free-standing comments and other keys' decor survive (spec OQ-1 resolution)
- [ ] Empty/absent document base: `apply_config_delta("", &Config::default(), &candidate)` yields a document whose `Config::load` parse equals `candidate` (spec AC-6 groundwork)
- [ ] Unparseable `doc_text` returns `DeltaError::Parse` naming the underlying toml_edit error — no panic, no silent fallback
- [ ] `table_section` returns the `[web]` table with its decor exactly as it appears in the edited document; `None` when absent
- [ ] Module is pure (no `std::fs`/`std::io` use); unit tests cover every bullet above and run under default features (`cargo test -p teton-core`)

## Technical Notes

- Diff both configs via `Config::to_toml()` → parse each with toml_edit →
  walk keys recursively. Never diff against the on-disk parse — that would
  clobber hand-edit drift (ADR-1 rationale).
- toml_edit 0.22 pairs with the workspace's toml 0.8.23 (lock already has
  toml_edit 0.22.27).
- Assigning a value to an existing key in toml_edit preserves the key's decor;
  inserting a new key gets canonical rendering — both acceptable per spec
  out-of-scope ("changed keys may re-render canonically").
- Keep `web_table_toml` (config.rs:531) untouched in this task; TASK-137
  decides its retirement.

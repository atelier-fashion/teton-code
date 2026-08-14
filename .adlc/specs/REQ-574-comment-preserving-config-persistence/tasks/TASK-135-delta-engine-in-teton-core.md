---
id: TASK-135
title: "Pure config-document delta engine in teton-core"
status: complete
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

- [x] `apply_config_delta` computes the delta by diffing canonical serializations of `current` vs `candidate` (recursive over tables; arrays — value arrays AND arrays-of-tables — are one key, replaced wholesale when different; keys in current-canonical but absent from candidate-canonical are removed from the document)
- [x] A fixture embedding the README's commented `[web]` block **verbatim** (incl. `search_auth = "X-Subscription-Token: {key}"`), plus an unknown key inside `[web]` and an unknown top-level table, survives a tier-only delta byte-for-byte outside the changed key (LESSON-512, spec AC-1 groundwork)
- [x] Removal semantics: removing a key removes its attached comment decor; free-standing comments and other keys' decor survive (spec OQ-1 resolution)
- [x] Empty/absent document base: `apply_config_delta("", &Config::default(), &candidate)` yields a document whose `Config::load` parse equals `candidate` (spec AC-6 groundwork)
- [x] Unparseable `doc_text` returns `DeltaError::Parse` naming the underlying toml_edit error — no panic, no silent fallback
- [x] `table_section` returns the `[web]` table with its decor exactly as it appears in the edited document; `None` when absent
- [x] Module is pure (no `std::fs`/`std::io` use); unit tests cover every bullet above and run under default features (`cargo test -p teton-core`)

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

## Implementation Notes (post-implementation)

Two behaviours are narrower than a literal reading of ADR-1, both to serve
BR-1 rather than to depart from it — flagged here for the review phase:

- **A table present on exactly one canonical side recurses against an empty
  table** instead of being inserted or removed whole. The `[web]` table is
  `skip_serializing_if = "WebConfig::is_unset"`, so the README's hand-written
  all-default block is *absent* from canonical(current); inserting the
  candidate's table wholesale on the first `/web setup` write would destroy
  exactly the comments this REQ exists to keep, and deleting the table when a
  setting reverts to its default would do the same. Per-key edits inside the
  table change precisely the keys the delta names and parse back identically.
  Scalars, value arrays and arrays-of-tables still set/remove wholesale, as
  ADR-1 specifies.
- **Placement of changed and added sections is pinned.** toml_edit renders
  top-level tables by a stored `position`; a canonical clone carries the
  *canonical* document's positions, which moved a replaced `[[providers]]`
  block's nested `[providers.capabilities]` to the far side of the user's
  `[web]` table. A replaced section (and everything nested in it) now inherits
  the position the document already gave it, and an added section renders past
  everything already in the file. Both are pinned by tests.

Also: the document's *value* decor is carried across a changed value, so a
trailing inline comment stays attached to the key it annotates (BR-1 names
inline comments explicitly), and `TargetTable` recurses into inline tables
(`web = { … }`) rather than replacing them, so unknown keys survive that
spelling too.

**Follow-up for the README-facing task (BR-7):** the fixture copies README.md's
hand-written `[web]` block byte-for-byte (verified against the file), so it now
belongs in the README's own drift-check comment (README.md:333-345) alongside
`self_config.md` and `web_setup_ui.rs`. Left to that task rather than edited
here, since this task's file list stops at teton-core.

`DeltaError` gained a second variant, `Serialize`, carrying `toml::ser::Error`
from `Config::to_toml` — unreachable for a well-formed config, and represented
rather than `expect()`ed because this is the one code path whose purpose is to
not lose the user's file.

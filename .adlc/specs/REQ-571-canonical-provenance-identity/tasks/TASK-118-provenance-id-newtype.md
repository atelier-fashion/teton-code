---
id: TASK-118
title: "Introduce the ProvenanceId newtype in teton-core"
status: complete
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: []
---

## Description

Add the type that makes a provenance source a minted identity rather than a
string (ADR-A). No consumers change in this task — it lands the type, its
constructors, and its unit tests so the migration in TASK-119 is a mechanical
narrowing rather than a design exercise.

## Files to Create/Modify

- `crates/teton-core/src/provenance_id.rs` — new. `ProvenanceId`, its two named constructors, well-formedness validation, and inline `#[cfg(test)]` coverage.
- `crates/teton-core/src/lib.rs` — declare and re-export the module.

## Acceptance Criteria

- [x] `ProvenanceId` exists with NO `From<String>`, `From<&str>`, or `Into<String>` impl — construction is possible only through the named constructors. A doc test or compile-fail note records that this is deliberate.
- [x] `ProvenanceId::from_resolved(root: &Path, resolved: &Path) -> Result<Self, ProvenanceError>` derives the id by `strip_prefix(root)` and normalizes separators to `/`. Returns `Err` when `resolved` is not under `root` — it never falls back to any other value (ADR-B).
- [x] `ProvenanceId::claimed(s: &str) -> Result<Self, ProvenanceError>` normalizes identically but is documented as a third-party assertion the daemon did not observe (for MCP arguments only).
- [x] Both constructors reject an id that is absolute or retains a `.`/`..` segment, returning a typed error.
- [x] `as_str()` exposes the canonical form for boundary matching.
- [x] Unit tests cover: each of the five BR-3 spellings resolving to one identical id; `strip_prefix` failure returning `Err`; absolute and `..`-bearing input rejected by both constructors; Windows-style separators normalized.
- [x] No filesystem access anywhere in the module — `teton-core` is a no-I/O crate (conventions.md).
- [x] `cargo clippy --all-targets` clean under workspace `deny` lints.

## Technical Notes

Path arithmetic only: `strip_prefix` + `to_string_lossy().replace('\\', "/")`,
mirroring the existing idiom at `crates/tetond/src/harness/tools/grep.rs:345`
and `glob.rs:92`. Canonicalization is the caller's job and stays in `tetond`.

Placement in `teton-core` is deliberate (ADR-A): boundary matching consumes the
type, and construction is pure. Do NOT add a convenience `impl Display`-backed
round trip that would let a `String` become a `ProvenanceId` implicitly.

## Implementation Notes (as landed)

Recorded for TASK-119, which consumes this type.

- **`.` is elided, `..` is rejected.** The two are not symmetric. `strip_prefix`
  is component-based, so `.` and repeated separators are *already* gone before
  `from_resolved` sees the remainder — the "reject on `.`" branch would be dead
  for the observing constructor and would only fire for `claimed()`, where it
  would make an MCP server's `./secrets/x` mint nothing and so miss a boundary
  the equivalent first-party read catches. `.` is therefore elided exactly as
  `Path::components` elides it, keeping both constructors naming one file
  identically. `..` is rejected outright and never collapsed: a lexical collapse
  is unsound through symlinks and would mint an id for a file that was never
  opened (BR-6, LESSON-494). Either way the resulting id *retains* neither
  segment, which is the invariant ADR-D's redundant guard depends on.
- **Four of the five BR-3 spellings need no filesystem.** Bare, `./`, `.//`,
  `././`, and absolute-inside-root all mint the identical id from a plain
  lexical join, so the unit test is non-vacuous. The fifth
  (`..`-traversing-but-inside-root) is refused un-canonicalized by design; its
  agreement with the other four is pinned at the tool layer, where
  canonicalization exists. The pure test asserts the invariant that matters
  here: every spelling yields *the* id or an `Err`, never a second identity.
- **No `Deserialize` derive**, deliberately — on a newtype it is a `From<String>`
  in disguise. Confirmed safe: `ToolProvenance` derives only
  `Debug, Clone, PartialEq, Eq`, so TASK-119's migration needs no serde.
- **`ProvenanceError` field is `path`, not `source`** — `thiserror` reads a field
  named `source` as the error's cause, which a `String` cannot satisfy.
- Both `compile_fail` doctests were verified to fail on the *specific* missing
  impls (`ProvenanceId: From<&str>`, `String: From<ProvenanceId>`) rather than
  incidentally, so neither is a vacuous pass.

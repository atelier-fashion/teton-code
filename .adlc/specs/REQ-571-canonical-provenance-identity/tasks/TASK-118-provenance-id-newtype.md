---
id: TASK-118
title: "Introduce the ProvenanceId newtype in teton-core"
status: draft
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

- [ ] `ProvenanceId` exists with NO `From<String>`, `From<&str>`, or `Into<String>` impl — construction is possible only through the named constructors. A doc test or compile-fail note records that this is deliberate.
- [ ] `ProvenanceId::from_resolved(root: &Path, resolved: &Path) -> Result<Self, ProvenanceError>` derives the id by `strip_prefix(root)` and normalizes separators to `/`. Returns `Err` when `resolved` is not under `root` — it never falls back to any other value (ADR-B).
- [ ] `ProvenanceId::claimed(s: &str) -> Result<Self, ProvenanceError>` normalizes identically but is documented as a third-party assertion the daemon did not observe (for MCP arguments only).
- [ ] Both constructors reject an id that is absolute or retains a `.`/`..` segment, returning a typed error.
- [ ] `as_str()` exposes the canonical form for boundary matching.
- [ ] Unit tests cover: each of the five BR-3 spellings resolving to one identical id; `strip_prefix` failure returning `Err`; absolute and `..`-bearing input rejected by both constructors; Windows-style separators normalized.
- [ ] No filesystem access anywhere in the module — `teton-core` is a no-I/O crate (conventions.md).
- [ ] `cargo clippy --all-targets` clean under workspace `deny` lints.

## Technical Notes

Path arithmetic only: `strip_prefix` + `to_string_lossy().replace('\\', "/")`,
mirroring the existing idiom at `crates/tetond/src/harness/tools/grep.rs:345`
and `glob.rs:92`. Canonicalization is the caller's job and stays in `tetond`.

Placement in `teton-core` is deliberate (ADR-A): boundary matching consumes the
type, and construction is pure. Do NOT add a convenience `impl Display`-backed
round trip that would let a `String` become a `ProvenanceId` implicitly.

---
id: TASK-119
title: "Narrow the provenance channel to ProvenanceId and migrate every tool"
status: draft
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-118]
---

## Description

Close the channel so a raw request string can no longer be tagged as
provenance (BR-1, BR-2). `.with_paths([raw])` must stop compiling. Includes the
spelling-matrix tests that prove the fix, because the migration without them is
an unverified claim.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/mod.rs` — `ToolContext::resolve` returns `Resolved { path, provenance }`; `with_paths` narrows from `S: Into<String>` to `ProvenanceId`.
- `crates/tetond/src/harness/context.rs` — `ToolProvenance::Sources(BTreeSet<ProvenanceId>)`; update `path()`/`paths()`/`none()`.
- `crates/tetond/src/harness/tools/read.rs` — tag from `Resolved::provenance`, not `raw`.
- `crates/tetond/src/harness/tools/edit.rs` — same.
- `crates/tetond/src/harness/tools/grep.rs` — replace the inline `strip_prefix` idiom with `ProvenanceId::from_resolved`.
- `crates/tetond/src/harness/tools/glob.rs` — same.
- `crates/tetond/src/harness/tools/mcp.rs` — argument-derived paths use `ProvenanceId::claimed`.
- `crates/tetond/src/harness/digest.rs` — update the `tool_result_provenance` bridge.
- `crates/tetond/src/egress/provenance.rs` — `Provenance` carries `ProvenanceId`.
- `crates/tetond/tests/egress_capture.rs` — add the spelling matrix.

## Acceptance Criteria

- [ ] `.with_paths([raw])` in `read.rs`/`edit.rs` is a **compile error**, not a lint.
- [ ] `read`/`edit` tag provenance from the resolved identity; a `strip_prefix` failure refuses the operation rather than falling back to `raw` (ADR-B).
- [ ] `grep`/`glob` produce byte-identical provenance to today for non-symlink files — the migration is a refactor for them, not a behavior change.
- [ ] AC-1: with boundary `secrets/**`, `read` is driven against all five BR-3 spellings; every one produces `privacy_block`, AND every one yields the byte-identical `provenance_id` (this is what pins BR-2).
- [ ] AC-1 positive control: a non-boundary file's content IS present in a captured payload, so the zero-leak assertion cannot pass vacuously (LESSON-479).
- [ ] AC-2: the same five-spelling matrix and both assertions applied to `edit`.
- [ ] AC-7: reverting provenance derivation to the raw argument in `read`, and separately in `edit`, each fails at least one test. Neither tool's coverage rides on the other's — use per-tool fixtures (LESSON-502).
- [ ] AC-9: `egress_capture`, `web_lookup_egress`, `mcp_egress`, `duty_egress`, `redact_egress`, `provenance_egress` all pass unchanged.
- [ ] `cargo clippy --all-targets` clean; `cargo test --workspace --no-fail-fast` green.

## Technical Notes

Extend the existing `CaptureTransport` + `CapturingSink` fixtures in
`crates/tetond/tests/egress_capture.rs:44-89` rather than inventing a harness.
The positive-control idiom is at `egress_capture.rs:211-213`.

`mcp.rs` is the one place `claimed()` is correct — the daemon cannot observe
what a remote server touched, and `mcp_egress.rs:428` pins that a boundary path
under an arbitrary key (`resource`, not `path`) is still caught. Preserve that.

Expect the compiler to enumerate the call sites; that is the mechanism, not an
inconvenience.

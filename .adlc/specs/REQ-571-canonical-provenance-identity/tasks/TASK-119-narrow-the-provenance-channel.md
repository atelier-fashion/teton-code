---
id: TASK-119
title: "Narrow the provenance channel to ProvenanceId and migrate every tool"
status: complete
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
- `crates/tetond/src/egress/inspector.rs`, `crates/tetond/src/egress/mod.rs` — test fixtures using `Provenance::tainted_by(&str)` follow the type change (production logic untouched here; TASK-122 owns its production edits).
- `crates/tetond/src/sessions.rs`, `crates/tetond/src/carry.rs`, `crates/tetond/src/runtime.rs` — `#[cfg(test)]` fixtures constructing `ToolProvenance::path/paths/none` from strings follow the type change. Verified at architect time these are the only non-listed constructor sites, and all are test-module-only.
- `crates/tetond/tests/egress_capture.rs` — add the spelling matrix.

## Acceptance Criteria

- [x] `.with_paths([raw])` in `read.rs`/`edit.rs` is a **compile error**, not a lint.
- [x] `read`/`edit` tag provenance from the resolved identity; a `strip_prefix` failure refuses the operation rather than falling back to `raw` (ADR-B).
- [x] `grep`/`glob` produce byte-identical provenance to today for non-symlink files — the migration is a refactor for them, not a behavior change.
- [x] AC-1: with boundary `secrets/**`, `read` is driven against all five BR-3 spellings; every one produces `privacy_block`, AND every one yields the byte-identical `provenance_id` (this is what pins BR-2).
- [x] AC-1 positive control: a non-boundary file's content IS present in a captured payload, so the zero-leak assertion cannot pass vacuously (LESSON-479).
- [x] AC-2: the same five-spelling matrix and both assertions applied to `edit`.
- [x] AC-7: reverting provenance derivation to the raw argument in `read`, and separately in `edit`, each fails at least one test. Neither tool's coverage rides on the other's — use per-tool fixtures (LESSON-502).
- [x] AC-9: `egress_capture`, `web_lookup_egress`, `mcp_egress`, `duty_egress`, `redact_egress`, `provenance_egress` all pass unchanged.
- [x] `cargo clippy --all-targets` clean; `cargo test --workspace --no-fail-fast` green.

## Technical Notes

Extend the existing `CaptureTransport` + `CapturingSink` fixtures in
`crates/tetond/tests/egress_capture.rs:44-89` rather than inventing a harness.
The positive-control idiom is at `egress_capture.rs:211-213`.

`mcp.rs` is the one place `claimed()` is correct — the daemon cannot observe
what a remote server touched, and `mcp_egress.rs:428` pins that a boundary path
under an arbitrary key (`resource`, not `path`) is still caught. Preserve that.

Expect the compiler to enumerate the call sites; that is the mechanism, not an
inconvenience.

## Implementation Notes (as landed)

Recorded for TASK-120/122/123, which build on this.

- **The six-spelling set, not five.** BR-3 names five; the matrix drives six,
  because `.//x` and `././x` are distinct spellings a model can emit and both had
  to be shown to collapse. The `..`-traversing one is the interesting member:
  `teton-core` refuses it un-canonicalized by design (TASK-118), so it is
  `ToolContext::resolve`'s canonicalization that makes it agree — the tool layer
  is where BR-3 actually closes.
- **`resolve` now refuses `.`.** Only `ProvenanceError::Empty` is reachable from
  the mint: the pre-existing `starts_with(&root)` check is `strip_prefix`'s
  precondition, so `NotUnderRoot` cannot fire, and `lexical_normalize` has already
  collapsed `.`/`..` so neither `Absolute` nor `ParentTraversal` can. `resolve(".")`
  therefore became a jail error rather than returning the root. Nothing in
  production called it that way (`read`/`edit` are the only callers, and both
  then opened the result as a file), so this only makes an already-failing call
  fail one step earlier and with a better sentence.
- **Call sites beyond the task list.** The `Provenance` narrowing rippled further
  than the file list anticipated. Production: `crates/tetond/src/mcp/client.rs`
  (`collect_paths`/`call_provenance` — the one production site that built egress
  provenance straight from raw argument strings, now minting through
  `ProvenanceId::claimed`). `#[cfg(test)]` fixtures: `harness/compact.rs`,
  `harness/completion.rs`, `harness/render.rs`, `harness/title.rs`,
  `harness/triage.rs`, `harness/tools/{read,edit,grep,glob,mcp}.rs`, and the
  integration tests `tests/{cost_attribution,duty_egress,duty_matrix,redact_egress,remote_loop,routing}.rs`.
- **An un-mintable *claimed* path fails toward taint.** An MCP argument that is
  absolute or `..`-bearing marks the whole call's provenance `Unknown` rather
  than contributing nothing. Dropping it would be failing open on exactly the
  value that could name a boundary file (`/repo/secrets/x`) — the BR-2 hole on
  the MCP path — and this is ADR-D's fail-closed posture landed at mint time
  rather than at inspection. It bites only when a boundary is configured, since
  the inspector runs nowhere else. TASK-122 adds the `provenance_rejected` event
  that reports it.
- **`push_tool_result` takes `Option<ProvenanceId>`.** It constructed a
  `ToolProvenance` from a raw `String`, i.e. the same hole one layer up. Every
  production call site passes `None`; only fixtures were affected.
- **One `#[cfg(test)] pub(crate) fn fixture_id`** at the crate root (`tetond/src/lib.rs`),
  the same posture as `RetainedContext::from_blocks`: production code that
  reaches for it does not compile. Integration tests cannot see it, so each of
  those binaries states its own `source_id`.

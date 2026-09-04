---
id: TASK-391
title: "Wire the verdict into the shell tool — ToolProvenance::BoundaryTouch and boundaries on ToolContext"
status: draft
parent: REQ-614
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-390]
---

## Description

Replace the four constant `.with_unknown_provenance()` calls in
`ShellTool::run` with the classifier's verdict, and give `ToolContext` the
boundary set the classifier needs. Adds the third `ToolProvenance` variant so
a `boundary_touch` survives the trip to the taint seam with the compiler
enumerating every place that must handle it.

The verdict is computed **before** the outcome is measured and before
`refine` hands anything to the `shell` duty, so the duty's interpretation and
any later digest inherit it (BR-10).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell.rs` — call `classify`; the four `with_unknown_provenance` sites become verdict-derived
- `crates/tetond/src/harness/context.rs` — `ToolProvenance::BoundaryTouch`
- `crates/tetond/src/harness/digest.rs` — `tool_result_provenance` maps `BoundaryTouch` to `Provenance::unknown()`
- `crates/tetond/src/harness/tools/mod.rs` — `ToolContext` carries `boundaries`; constructor and `with_*` seams

## Acceptance Criteria

- [ ] `ToolProvenance::BoundaryTouch` exists and `tool_result_provenance` maps it to `Provenance::unknown()` — egress behavior byte-identical to `Unknown`, so BR-2's fail-closed refusal is unchanged by construction
- [ ] All four spawned-command arms of `ShellTool::run` (`Completed`, `Lost`, `TimedOut`, and the `SpawnFailed` arm's unchanged no-provenance posture) derive provenance from the verdict; `SpawnFailed` still carries none — nothing ran
- [ ] A `Rooted` verdict produces `ToolProvenance::Sources(verdict.sources)` — exactly what a `glob` over the same paths would produce (BR-1)
- [ ] `TimedOut` and `Lost` carry the **same** verdict a completed run would (BR-8): a `pwd` that timed out is `Rooted`, a `curl` that failed is `Unknown`
- [ ] The verdict is computed in `run`, before `measuring(...)` and before `refine` (BR-10)
- [ ] `ToolContext` carries the effective boundary set; every constructor that builds a context for a live session populates it from `Config::effective_boundaries()`
- [ ] With no boundaries configured the tool's behavior is unchanged from today (BR-9)
- [ ] The existing test `any_shell_result_carries_unknown_provenance` is **replaced**, not deleted silently: the new test asserts the verdict-derived provenance for each of the four arms and names in its doc comment what the old test asserted and why it no longer holds

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_rooted_command_carries_its_resolved_sources` | yes |
| BR-2 | test-case | `crates/tetond/src/harness/tools/shell.rs::an_unknown_verdict_still_maps_to_unknown_provenance` | yes |
| BR-8 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_timed_out_pwd_is_still_rooted` | yes |
| BR-9 | test-case | `crates/tetond/src/harness/tools/shell.rs::no_boundaries_configured_changes_nothing` | yes |
| BR-10 | structural-check | `crates/tetond/src/harness/tools/shell.rs::the_verdict_is_computed_before_measurement` — bounded region check over `run`'s body | no |
| AC-1 | test-case | `crates/tetond/src/harness/tools/shell.rs::ls_la_from_a_project_root_is_rooted` | yes |
| AC-4 | test-case | `crates/tetond/src/harness/tools/shell.rs::a_timed_out_pwd_is_still_rooted` | yes |

## Technical Notes

- Keep the diff on `shell.rs` minimal (ADR-614-7): REQ-615 rewrites this
  file's tool description and adds a cwd note in the same sprint. Four call
  sites plus one `use` rebases cleanly; a large insertion does not.
- The BR-10 region check must **bound its slice to `run`'s body** and cut the
  corpus at the first column-0 `#[cfg(test)]` (conventions.md, REQ-600). An
  unbounded `&source[start..]` is a claim about the rest of the file.
- Adding the `ToolProvenance` variant will produce compile errors at every
  exhaustive match. Fix each by deciding deliberately; do not add a catch-all
  `_ =>` arm, which is the thing that would let the next variant slip through
  unhandled.

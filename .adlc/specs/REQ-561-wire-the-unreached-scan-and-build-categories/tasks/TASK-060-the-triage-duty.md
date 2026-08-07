---
id: TASK-060
title: "The triage duty — rank grep matches before they enter context"
status: complete
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: [TASK-058]
---

## Description

Wire `Category::Triage` (scan tier). `GrepTool::run` returns the first 200
matches unranked; the duty ranks/filters them by relevance to the turn before
they enter context. On any failure the tool returns today's first-200 behaviour
verbatim (BR-3).

Lowest-risk of the four: the fallback *is* the current behaviour, so a total
duty failure is indistinguishable from today.

## Files to Create/Modify

- `crates/tetond/src/harness/triage.rs` — **new**. The triage prompt builder, `TRIAGE_OUTPUT_CONTRACT`, and `TRIAGE_OUTPUT_MAX_BYTES`.
- `crates/tetond/src/harness/tools/grep.rs` — call the duty after collecting matches (line ~93, where `hits.truncate(MAX_MATCHES)` runs); degrade to the unranked truncation on failure.
- `crates/tetond/src/harness/mod.rs` — declare the `triage` module.
- `crates/tetond/src/runtime.rs` — add `triage_route()` spelling `router.resolve(Category::Triage)` literally, delegating to the shared `resolve_duty()`.
- `crates/tetond/src/call_sites.rs` — flip `Category::Triage` to `true` in the `has_call_site()` match (~line 33-42).

## Acceptance Criteria

- [ ] `router.resolve(Category::Triage)` appears literally in `runtime.rs`, and the `call_sites.rs:209` derived-marker scan finds it. The literal is the BR-1 call-site tag — the category is named in source, never derived from prompt text or a tool name.
- [ ] Emits `route_decided` naming category, tier, provider, non-empty reason (AC-2).
- [ ] Every failure path (unresolvable / provider error / tainted session) returns the current first-200 unranked result, and the degradation is visible **on the outcome**, not only in a log (BR-3).
- [ ] Egress is scoped to the **matched files'** provenance, not the turn's (BR-7). A `local-only` match refuses while the rest of the turn proceeds.
- [ ] Output bounded by `TRIAGE_OUTPUT_MAX_BYTES`; the test reads the constant, never a literal (BR-8, AC-11).
- [ ] `ScriptedFileEngine` gains a `TRIAGE_OUTPUT_CONTRACT` arm; a test asserts the duty consumes **no** scripted block and the turn sequence is unchanged (AC-12, BR-10).
- [ ] A test asserts the prompt carries `TRIAGE_OUTPUT_CONTRACT` verbatim — mirroring `the_duty_prompt_carries_the_output_contract_verbatim` at `context.rs:1104`.
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Technical Notes

`MAX_MATCHES = 200` at `tools/grep.rs:20`, truncation at `:93`, returns
`ToolOutcome::ok(out).with_paths(matched_files)` at `:98`. Those `matched_files`
are the provenance source for BR-7 (LESSON-432: provenance comes from the files,
not the argument name).

**The ScriptedFileEngine arm is not optional and not deferrable.** `runtime.rs:263`
dispatches on `prompt.contains(<CONTRACT>)`. A duty with no arm consumes a
scripted block and shifts every fixture's turn sequence by one — REQ-558 was bitten
by this twice. Add the arm in this task, with its test.

Ranking must be stable and total: the duty may reorder and filter, but the tool's
own cap still applies afterward, so a duty that returns more than 200 cannot
inflate the result.

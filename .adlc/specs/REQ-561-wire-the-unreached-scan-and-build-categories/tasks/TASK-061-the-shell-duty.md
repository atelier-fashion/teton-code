---
id: TASK-061
title: "The shell duty — interpret command output on failure or oversize"
status: draft
parent: REQ-561
created: 2026-08-07
updated: 2026-08-07
dependencies: [TASK-058, TASK-060]
---

## Description

Wire `Category::Shell` (build tier). The duty interprets command output — what
failed and what it means — but only when the command exits non-zero **or** its
raw output exceeds the 8,000-char cap (BR-4b, OQ-1 resolved). A short successful
command is returned verbatim with no model call.

The negative case is the whole cost argument: `shell` is the highest-frequency
tool call in a coding session.

## Files to Create/Modify

- `crates/tetond/src/harness/shell_duty.rs` — **new**. Prompt builder, `SHELL_OUTPUT_CONTRACT`, `SHELL_OUTPUT_MAX_BYTES`, and the trigger predicate. Named `shell_duty` to avoid colliding with the existing `harness/tools/shell.rs`.
- `crates/tetond/src/harness/tools/shell.rs` — evaluate the trigger on the **raw** stdout+stderr before `render_output()` truncates (line ~228/241); call the duty when it fires; degrade to today's 8k truncation on failure.
- `crates/tetond/src/harness/mod.rs` — declare the `shell_duty` module.
- `crates/tetond/src/runtime.rs` — add `shell_route()` spelling `router.resolve(Category::Shell)` literally.
- `crates/tetond/src/call_sites.rs` — flip `Category::Shell` to `true`.

## Acceptance Criteria

- [ ] **AC-13, the load-bearing one**: a table-driven test over (exit 0, small), (exit 0, >8k), (exit≠0, small), (exit≠0, large) asserts the duty is invoked in exactly the last three and **not** in the first, by call count. The zero-call case is the assertion that matters.
- [ ] The trigger reads the **raw** output length, not the post-truncation length (ADR-5). A mutation moving the check after `render_output()` must turn AC-13's oversize case red.
- [ ] `router.resolve(Category::Shell)` appears literally; the derived-marker scan finds it.
- [ ] Emits `route_decided` (AC-2).
- [ ] Every failure path returns today's 8k-truncated output with degradation visible on the outcome (BR-3).
- [ ] Egress scoped to the command output's own provenance (BR-7). Shell output is tagged `with_unknown_provenance()` today (`tools/shell.rs:140,145,164,168`) — so it fails closed, which is correct and must be asserted rather than worked around.
- [ ] Bounded by `SHELL_OUTPUT_MAX_BYTES`, test reads the constant (AC-11).
- [ ] `ScriptedFileEngine` arm + no-block-consumed test (AC-12, BR-10) + contract-verbatim test.
- [ ] `cargo test --workspace --no-fail-fast` is green.

## Technical Notes

`MAX_OUTPUT_CHARS = 8_000` at `tools/shell.rs:36`, enforced in `render_output()`
at `:241-243`. **Capture the raw length before calling `render_output()`** — see
ADR-5. Comparing a post-truncation length against the cap that produced it is a
guard that can never fire (LESSON-443).

Shell output carries `ToolProvenance::Unknown`, which `Provenance::unknown()`
makes the egress choke point block by default (BUG-156 confirmed `inspect` blocks
on `provenance.is_unknown()` *before* any glob). So a remote-bound `shell` duty
will be refused unless the daemon can attribute the output. Do **not** loosen this
to make the duty work — assert the fail-closed behaviour and let the duty degrade.
That is BR-3 operating correctly, not a bug.

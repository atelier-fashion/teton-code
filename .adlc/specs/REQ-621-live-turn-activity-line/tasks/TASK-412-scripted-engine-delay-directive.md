---
id: TASK-412
title: "A debug-only @delay-ms directive in the scripted engine, and a shell block that sleeps"
status: draft
parent: REQ-621
created: 2026-09-10
updated: 2026-09-10
dependencies: []
repo: teton-code
---

## Description

Give the pty legs a deterministic way to hold a turn open (ADR-621-6).
`ScriptedFileEngine::complete` reads an optional first line `@delay-ms <n>` in a
reply block, sleeps that long before the first token, and strips the line.
Honoured only when `TETON_TEST_SEAMS=1` in a debug build — the gate the other
seams in `engine.rs` use; elsewhere the line is streamed verbatim, so a script
cannot silently change meaning outside the seam. Document the directive beside
`SCRIPT_SEPARATOR`.

## Files to Create/Modify

- `crates/tetond/src/runtime/mod.rs` — `SCRIPT_DELAY_DIRECTIVE = "@delay-ms"`; parse/strip in `complete` under the seam gate; doc comment on `ScriptedFileEngine`; unit tests
- `crates/tetond/src/runtime/engine.rs` — expose the existing seam predicate as `pub(crate) fn test_seams_enabled() -> bool` if it is not already callable from `mod.rs`

## Acceptance Criteria

- [ ] A block `@delay-ms 250\nhello` under the seam streams `hello` no sooner than 250 ms after `complete` is entered, and never streams the directive
- [ ] The same block with the seam off streams `@delay-ms 250\nhello` verbatim
- [ ] A malformed directive (`@delay-ms x`) is streamed verbatim under the seam too — it is not a delay
- [ ] The duty arms (redaction, digest, title, …) are unaffected: they answer off-script before the directive is read
- [ ] `cargo test -p tetond runtime::` green; clippy and fmt clean

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| AC-11 | test-case | `crates/tetond/src/runtime/mod.rs::tests::the_delay_directive_is_honoured_only_under_the_seam` | yes |

## Technical Notes

- The sleep must run on the blocking engine thread the local tier already uses; `complete` is synchronous, so a plain `std::thread::sleep` is correct here.
- No new env var: the directive lives in the script text, gated by the seam predicate already read at engine construction. Do not add a second gate spelling (LESSON-494).
- AC-2's running tool needs no seam: a scripted `{"tool": "shell", "arguments": {"command": "sleep 3"}}` block, followed by a plain reply block, the same shape as `cli_e2e.rs::SHELL_CALL`.

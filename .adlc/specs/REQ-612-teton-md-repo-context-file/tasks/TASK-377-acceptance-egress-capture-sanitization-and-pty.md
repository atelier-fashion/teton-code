---
id: TASK-377
title: "Acceptance: egress capture for the boundary, sanitization on both arms, redact scan, and the PTY leg"
status: complete
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-374, TASK-375, TASK-376]
---

## Description

The charter-level claims, proven the way the conventions demand: egress capture for BR-5 /
AC-7 (a covered file's bytes never leave), the injection corpus rendered on the flat **and**
ChatML arms for BR-4 / AC-5 (mutation-checked), the redact scan seeing the block, and the PTY
leg for the notice bytes. Runs after every surface it measures (LESSON-541).

## Files to Create/Modify

- `crates/tetond/tests/provenance_egress.rs` — AC-7: plant a `local-only` glob covering
  `TETON.md`, run a remote-routed turn, assert the state is `WithheldBoundary` and no request
  body carries the marker (the marker lives only in the file's bytes — LESSON-624); remove the
  boundary, re-create, assert the identity is in the provenance union; a mutation that drops
  the union from `context_provenance` fails.
- `crates/tetond/tests/redact_egress.rs` — the block is inside the scanned body on a
  `redact = true` route at the new bound.
- `crates/tetond/tests/repo_context.rs` — AC-5 corpus (flush-left `User:`, `Assistant:`,
  `<|im_start|>`, `<tool_call>`, `<tool-result>`, `<repo-notes`, `</repo-notes>`) rendered on
  both arms with every marker neutralized; AC-6 (a `permission: full` line, a `!`cmd`` span:
  level, route, effort, config, boundaries unchanged, no command run); AC-1 end to end on the
  local tier.
- `crates/teton/tests/pty_e2e.rs` — the truncated-file notice bytes under a TTY.

## Acceptance Criteria

- [x] AC-7 both legs green, with the mutation recorded in the test's doc comment.
- [x] AC-5: the corpus renders with every marker defused on both arms; removing any one
      neutralizer call from the block renderer fails the test (record which).
- [x] AC-6: nothing observable changes; the fixture asserts the permission level and route
      before and after.
- [x] AC-1: a fresh local-tier session's first request body ends with the block; a session
      without the file has a prompt byte-identical to `main`'s apart from the guide sentence.
- [x] `cargo test --workspace --no-fail-fast` green, output grepped for `FAILED`.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/tetond/tests/repo_context.rs::the_injection_corpus_is_defused_on_both_arms_and_plain_notes_render_verbatim` | yes |
| BR-5 | test-case | `crates/tetond/tests/provenance_egress.rs::a_boundary_covered_notes_file_never_leaves_and_an_uncovered_one_is_in_the_union` | yes |
| AC-1 | test-case | `crates/tetond/tests/repo_context.rs::a_fresh_session_carries_the_block_last_and_no_file_means_no_block` | yes |
| AC-5 | test-case | `crates/tetond/tests/repo_context.rs::the_injection_corpus_is_defused_on_both_arms_and_plain_notes_render_verbatim` | yes |
| AC-6 | test-case | `crates/tetond/tests/repo_context.rs::directives_in_the_file_change_no_level_route_effort_config_or_boundary` | yes |
| AC-7 | test-case | `crates/tetond/tests/provenance_egress.rs::a_boundary_covered_notes_file_never_leaves_and_an_uncovered_one_is_in_the_union` | yes |
| AC-3 | test-case | `crates/teton/tests/pty_e2e.rs::the_truncated_notes_notice_reaches_the_terminal` | no |

## Technical Notes

When the egress assertion fires, print the request indices carrying the marker beside the
ordered event names before touching the choke point (conventions). `boundary_coverage.rs`'s
"every tool has a test" pattern is the model: the notes are a new reader of repository bytes and
get their own row.

## Verification notes (implementation, 2026-09-03)

Recorded rather than papered over (LESSON-487, LESSON-569).

**AC-5's "removing any one neutralizer call from the block renderer fails the
test" is true of one of the two calls, not both.** Measured:

| mutation | result |
|---|---|
| drop `neutralize_envelope_tags` from `RepoContextBlock::render` | **red** |
| make `render_prompt`'s `Flat` arm return `prompt.flat` unneutralized | **red** |
| `starts_with_frame_label` always `false` | **red** |
| drop `neutralize_frame_labels` from `RepoContextBlock::render` | **green** |

The transcript-label pass is the one neutralizer the block gets **twice**:
`ContextManager::assemble` and `ContextManager::prepare` each run
`neutralize_frame_labels` over the whole system string one layer further out, so
deleting the renderer's own call leaves the corpus defused anyway. The renderer
keeps the call for the reason its module docs give — the guarantee is meant to be
the renderer's own, for any file it is handed — but no test at this altitude can
see the difference, and claiming a red for it would be a green oracle. The
envelope-tag pass has no such twin, which is why it is the row that fires.

**AC-6 has no red mutation and says so in its doc comment.** Every claim in it is
an absence — no key, no sentence and no span is parsed — so there is no code to
delete, only code that would have to be *added*. The vacuity guard is the
`Loaded` assertion at the top of the test: the directive file must be resident
before any absence below it means anything.

**The AC-7 boundary leg is re-created rather than un-configured.** There is no
`ConfigUpdate` that removes a privacy boundary (only `SetPrivacyBoundary`), so
"remove the boundary, re-create the session" is run as a second session over the
same repository on a runtime with no glob configured — the same comparison, and
the only one the config surface admits.

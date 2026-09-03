---
id: TASK-368
title: "Deny the transcript directory in the tool jail and the walkers"
status: complete
parent: REQ-611
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-360]
---

## Description

BR-8's mechanism (architecture ADR-7): the session's effective transcript directory becomes a
**denied prefix** at the two seams that can see a path — `ToolContext::resolve`, which every
`read`/`edit` and every walker seed passes through, and `WalkPolicy`, which `walk::visit` prunes
by. A path under the prefix is refused with a reason that names it as a transcript, inside or
outside the session root. Landed before the sink is wired so the refusal exists before the first
file does. Covers BR-8 and the unit legs of AC-12 and AC-21.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/mod.rs` — `ToolContext.denied_prefixes: Vec<PathBuf>`
  (canonicalized in the constructor; a prefix that does not exist yet is kept lexically
  normalized and re-canonicalized on first hit), `ToolContext::with_denied_prefix`. In `resolve`
  (line ~309), after the outside-root check and before `ProvenanceId::from_resolved`: if
  `checked.starts_with(prefix)` for any prefix, return `ToolError::jail("path `{raw}` is a
  session transcript; tools do not read transcripts")`.
- `crates/tetond/src/harness/tools/walk.rs` — `WalkPolicy.denied_prefixes`, consulted in
  `visit` beside `skip_dirs()`: a directory whose canonical path equals or is under a prefix is
  pruned and counted in the report as skipped, never listed, never entered.
- `crates/tetond/src/runtime/turn.rs` (or wherever `ToolContext`/`WalkPolicy` are built per
  turn) — pass the session's effective transcript directory from `TranscriptConfig::effective_dir`
  and the resolved data dir. Passed whenever a directory is known, not only while recording.

## Acceptance Criteria

- [x] BR-8 (jail seam): `resolve("<dir>/x.jsonl")` with the dir **inside** a temp root is
      refused with the transcript reason; the same call with the dir **outside** the root is
      refused (the existing out-of-root reason is acceptable — assert refusal, and assert the
      transcript reason when the path is in-root). A sibling path beside the dir resolves
      normally (benign path).
- [x] BR-8 (walker seam): `walk::visit` over a root containing the transcript dir never yields a
      path under it and reports it as skipped; a sibling directory is walked (benign path).
- [x] AC-12 (unit legs): `read`, `edit`, `grep` and `glob` each refuse an in-root transcript path
      through their own entry point — four cases, not one, because the seams differ (LESSON-502).
- [x] AC-21 (unit leg): the four refusals are identical with `disable_default_boundaries = true`;
      the denial reads no boundary state.
- [x] Mutation recorded: removing the check from `resolve` reddens the `read`/`edit` cases and
      **not** the walker case; removing it from `WalkPolicy` reddens `grep`/`glob` and not
      `read`/`edit`. Both results written into the tests' doc comments.
- [x] `boundary_coverage.rs`'s every-tool-has-a-test posture is satisfied: each file tool has a
      transcript-refusal case.
- [x] `cargo test -p tetond harness::tools --no-fail-fast` is green.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-8 | test-case | `crates/tetond/src/harness/tools/mod.rs::resolve_refuses_a_transcript_path_and_admits_its_sibling` | yes |
| BR-8 | test-case | `crates/tetond/src/harness/tools/walk.rs::visit_prunes_a_denied_prefix_and_walks_its_sibling` | yes |
| AC-12 | test-case | `crates/tetond/src/harness/tools/mod.rs::each_file_tool_refuses_an_in_root_transcript` | no |
| AC-21 | test-case | `crates/tetond/src/harness/tools/mod.rs::the_transcript_denial_ignores_the_boundary_set` | yes |

## Technical Notes

Do this in the jail, not as a boundary (ADR-7): provenance ids are root-relative and cannot
name a file outside the root, and inside the root a boundary would taint a read that must not
happen. The refusal text is the user's whole mental model — keep it to one sentence and reuse the
`ToolError::jail` constructor so `/verbose` and the tool result render it like an out-of-root
refusal.

`shell` is untouched by design (REQ-596 BR-6). Do not add a `cat` filter; the spec's Out of
Scope names the sandbox as the real fix.

Canonicalize prefixes the way `resolve` canonicalizes candidates (`canonical_through_existing_
ancestor`), or a symlinked data dir on macOS (`/var` → `/private/var`) defeats `starts_with`.

## Outcome

Landed as specified. Three notes for the tasks downstream of this one:

- **The effective transcript directory has one composer**:
  `crates/tetond/src/runtime/turn.rs::effective_transcript_dir(&TranscriptConfig)
  -> PathBuf`, `pub(super)`, composing `TranscriptConfig::effective_dir` with
  `teton_protocol::socket_path::data_dir()`. TASK-363 constructs the sink in
  `runtime/mod.rs`, which `pub(super)` reaches. A consumer outside `runtime/`
  promotes it to `pub(crate)` **and** registers it in
  `tests/runtime_visibility.rs`'s `CRATE_WIDE` — that suite fails the widening
  otherwise, which is how the visibility was chosen rather than guessed.
- **`boundary_coverage.rs` needed no new entry.** Its `COVERAGE` table pairs each
  tool with an *egress boundary* test, and ADR-7's denial is deliberately not a
  boundary. The posture the AC asks for is satisfied by the four per-tool legs
  inside `each_file_tool_refuses_an_in_root_transcript` — one case per tool, not
  one case standing for four (LESSON-502). The suite is green unchanged,
  including `no_walker_declares_a_private_skip_list_and_both_walk_through_the_shared_driver`.
- **`WalkReport` gained `denied: usize` and deliberately no trailer line.** Every
  other thing the report carries is rendered for the model; naming a pruned
  transcript directory in tool output would hand back the fact the denial hides
  (BR-15). The count exists so a test can tell a pruned tree from an absent one.

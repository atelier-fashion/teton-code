---
id: TASK-371
title: "The `repo_context` module: load, bound, strip, truncate, frame, and mint provenance"
status: draft
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: [TASK-372]
---

## Description

The feature's core as a pure-as-possible module with no wiring (ADR-3, ADR-4, ADR-5): given a
probed root, a boundary matcher and the switch, produce a `RepoContextState`; given a loaded
file, produce the `RepoContextBlock` text. Filesystem access is behind a `RepoFileReader` trait
(the `DirLister` shape) so every rule here is unit-tested with an in-memory fixture. Covers
BR-1 (what is read), BR-3 (cap and truncation), BR-4 (strip and frame), and the load-time half
of BR-5 (mint the identity, refuse a covered file).

## Files to Create/Modify

- `crates/tetond/src/repo_context/mod.rs` — `RepoContextState` (the six states, with the
  loaded file for `Loaded`/`Truncated`), `RepoContextSource`, `RepoContext::load`,
  `RepoContext::refresh` (stat compare + boundary re-check → `Option<RepoContextState>`),
  the `RepoFileReader` trait and its `std::fs` impl, `CANDIDATE_NAMES = ["TETON.md",
  "AGENTS.md"]`, `REPO_CONTEXT_MAX_BYTES = 8_192`, `REPO_CONTEXT_READ_CEILING_BYTES = 65_536`.
- `crates/tetond/src/repo_context/render.rs` — `RepoContextBlock { text, provenance:
  ProvenanceId, truncated, resident_bytes }`, `RepoContextBlock::render(file, effective_cap) -> Self` (the cap is a parameter — the route decides it, ADR-5),
  `RepoContextBlock::worst_case()` (a block synthesized at the cap for the two sweeps),
  strip (C0 except `\n`/`\t`, bidi overrides U+202A–U+202E and U+2066–U+2069), truncate at the
  last `\n` at or under the cap, frame lines per ADR-4 with `neutralize_frame_labels` and
  `neutralize_envelope_tags` applied to the text as the frame is written.
- `crates/tetond/src/lib.rs` — `pub mod repo_context;`.

## Acceptance Criteria

- [ ] BR-1: only a `project`-kind root is read; `TETON.md` wins over `AGENTS.md`; a symlinked
      entry is not followed (state `Unreadable` naming the reason); `EPERM` is `Unreadable`
      with a bounded reason; a missing file is `Absent` after exactly one `stat` per candidate
      and no read; nothing above or below the root is opened (the fixture records every path
      asked for).
- [ ] BR-3: a file of cap + 1 byte is `Truncated` at the last `\n` under the cap with the
      marker naming the cap and the bytes dropped; a file exactly at the cap is `Loaded` whole
      with no marker; a 10 MiB file is read to `REPO_CONTEXT_READ_CEILING_BYTES` and no further
      (the fixture counts bytes served).
- [ ] BR-4: control characters and bidi overrides are stripped **before** the cap is measured
      (a file of 8,192 printable bytes plus 500 NULs is `Loaded` whole); the frame lines are
      byte-exact against a golden; `escape_attribute` is used for the file attribute.
- [ ] BR-5 (load half): a covered identity (planted `local-only` glob) yields
      `WithheldBoundary` with no block; an uncovered one yields a block whose `provenance` equals
      `ProvenanceId::from_resolved(root, path)`; the switch off yields `WithheldOff` with zero
      reader calls.
- [ ] `refresh` returns `None` when `mtime` and `len` match and the boundary verdict is
      unchanged; `Some` on either change; it never reads when it returns `None`.
- [ ] `worst_case().text.len()` equals the rendered length of a cap-sized file plus the frame —
      asserted against `render` of a synthesized file, not a literal.
- [ ] Rendering the same file at an effective cap of 4,096 (a floored route) truncates at that
      figure and the marker names 4,096, not 8,192.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-1 | test-case | `crates/tetond/src/repo_context/mod.rs::only_a_project_root_is_read_and_teton_md_wins_over_agents_md` | yes |
| BR-1 | test-case | `crates/tetond/src/repo_context/mod.rs::a_symlinked_entry_and_an_eperm_are_named_unreadable_and_a_missing_file_is_absent_after_one_stat` | yes |
| BR-3 | test-case | `crates/tetond/src/repo_context/render.rs::cap_plus_one_truncates_at_a_line_boundary_with_the_marker_and_cap_exactly_is_whole` | yes |
| BR-3 | test-case | `crates/tetond/src/repo_context/mod.rs::the_read_stops_at_the_read_ceiling` | no |
| BR-4 | test-case | `crates/tetond/src/repo_context/render.rs::controls_and_bidi_are_stripped_before_the_cap_and_the_frame_is_golden` | yes |
| BR-5 | test-case | `crates/tetond/src/repo_context/mod.rs::a_boundary_covered_file_is_withheld_and_an_uncovered_one_mints_its_identity` | yes |
| BR-6 | test-case | `crates/tetond/src/repo_context/mod.rs::refresh_reads_only_when_mtime_len_or_verdict_changed` | yes |
| AC-3 | test-case | `crates/tetond/src/repo_context/render.rs::cap_plus_one_truncates_at_a_line_boundary_with_the_marker_and_cap_exactly_is_whole` | yes |
| AC-6 | test-case | `crates/tetond/src/repo_context/render.rs::frontmatter_and_directives_in_the_file_are_text_and_change_nothing` | yes |

## Technical Notes

Resolve the candidate through the same outside-root and symlink refusals `ToolContext::resolve`
applies (`harness/tools/mod.rs`) — call that helper rather than re-spelling it. The boundary
matcher is `teton_core::boundary::BoundaryMatcher::match_path` on the minted id (LESSON-623: the
identity is minted by the resolving seam). Never call into `projects::scan` — `scan.rs:525`'s
forbid list guards the create derivation. Depends on TASK-372 for the delimiter constants; a
compile error on those means wait, not fix (LESSON-541).

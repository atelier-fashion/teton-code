---
id: TASK-404
title: "Segments carry their position; a piped reader with no path reads stdin, not the root"
status: draft
parent: REQ-620
created: 2026-09-09
updated: 2026-09-09
dependencies: ["TASK-403"]
---

## Description

Replace the uniform `split(['|', ';', '&', '(', ')', '\n'])` with a splitter that yields
`(SegmentPosition, &str)` — `Piped` only after a single `|` — and thread the position into
`classify_segment`, where the root-walk condition exempts a piped content verb with no path
argument unless it is recursive `grep` (ADR-620-2 step 3, ADR-620-3).

## Files to Create/Modify

- `crates/tetond/src/harness/tools/shell_provenance.rs` — `SegmentPosition`, the position-aware splitter, `reads_tree(verb, words)`, the amended root-walk arm, docs and mutation record; tests

## Acceptance Criteria

- [ ] On a fixture root holding a boundary-matching file: `ls src | head -5` → `Rooted`; `ls src | wc -l` → `Rooted`; `git log | grep fix` → `Rooted`
- [ ] Same root: `ls src | grep -r foo`, `ls src | grep -rn foo`, `ls src | grep --recursive foo`, `ls src | grep -d recurse foo` → `Unknown` "reads the root"
- [ ] Same root: `head -5` alone, and `ls; head -5` → `Unknown` "reads the root" (a `First` segment)
- [ ] `ls || head -5` treats `head` as `First` (an *or* is not a pipe)
- [ ] The splitter's mutation record names the test that goes red when `Piped` is returned for every separator, and the one that goes red when `reads_tree` is deleted

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::a_piped_reader_with_no_path_reads_stdin_not_the_root` | yes |
| BR-4 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::recursive_grep_reads_the_tree_whatever_its_stdin` | yes |
| AC-5 | test-case | `crates/tetond/src/harness/tools/shell_provenance.rs::tests::a_piped_reader_with_no_path_reads_stdin_not_the_root` | yes |

## Technical Notes

- Tokenise separators before splitting: scan the residue for `||`, `&&`, `|`, `;`, `&`,
  `(`, `)`, `\n` longest-match-first; `||` and `&&` are two-character separators that must
  not be read as `|` or `&`. Today's byte-wise `split` treats `||` as two empty-segment
  boundaries — harmless for verdicts, wrong for position.
- Only `paths.is_empty()` qualifies for the exemption: `cat missing | head` still walks in
  the first segment (`cat missing`), and that is BR-4's stated limit, not a gap.
- Short-flag clusters: any word starting with a single `-` whose remaining characters
  contain `r` or `R` counts as recursive for `grep`/`egrep`/`fgrep`; `--recursive` and the
  pair `-d recurse` are the long forms. Do not widen to other verbs.
- The fixture root for the "reads the root" rows must contain a file the builtin
  boundaries match (`.env` suffices) so the walk *hits* rather than merely truncating —
  LESSON-485: a fixture that cannot reach the discriminating state is not a test.

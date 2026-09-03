---
id: TASK-372
title: "The `<repo-notes>` delimiter pair joins the input alphabet and both output marker sets"
status: complete
parent: REQ-612
repo: teton-code
created: 2026-09-03
updated: 2026-09-03
dependencies: []
---

## Description

ADR-4's two-sided change: the new frame's opening and closing tags are declared once in
`render.rs`, added to `UNTRUSTED_ENVELOPE_TAGS` (input) and to `FLAT_ANCHORED_MARKERS` /
`CHATML_ANCHORED_MARKERS` (output), and the bidirectional coverage test names the layer. No
caller yet; TASK-371 imports the constants.

## Files to Create/Modify

- `crates/tetond/src/harness/render.rs` — `pub(crate) const REPO_NOTES_OPEN_TAG: &str =
  "<repo-notes"` and `REPO_NOTES_CLOSE_TAG: &str = "</repo-notes"`; both in
  `UNTRUSTED_ENVELOPE_TAGS`; coverage test (`render.rs:869–930`) gains the layer row.
- `crates/tetond/src/harness/reply.rs` — both output marker sets gain the opening tag (closing
  tags stay input-only by construction, BUG-151's rule).

## Acceptance Criteria

- [ ] A flush-left `<repo-notes file="x">` and `</repo-notes>` inside untrusted content are
      neutralized by `neutralize_envelope_tags` (insertion-only, `_` interposed); an indented
      one is untouched; ordinary prose is byte-identical.
- [ ] A model reply that emits `<repo-notes` is caught by the fabrication guard on both arms.
- [ ] The coverage test asserts the opening tag is in both output sets and claimed by exactly
      one input layer; deleting either side fails it with a message naming the layer (record
      the mutation in the test's doc comment).

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-4 | test-case | `crates/tetond/src/harness/render.rs::repo_notes_tags_are_defused_on_input_and_claimed_by_one_layer` | yes |
| AC-5 | test-case | `crates/tetond/src/harness/render.rs::the_input_alphabet_covers_every_output_marker` | no |

## Technical Notes

Sanitize by insertion, never deletion (LESSON-474). The constants live here, not in
`repo_context`, because this module owns the alphabet (LESSON-477 rule 3).

---
id: TASK-396
title: "Print the window beside the budget, with the currency named, on every surface"
status: complete
parent: REQ-616
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-392]
---

## Description

Remote routes do not change size; they change description. Every surface that
prints a budget prints the window first in the provider's own tokens, then the
derived budget with its currency named, so `1,000,000` no longer reads as
`665,984` (BR-6, AC-2, LESSON-446).

## Files to Create/Modify

- `crates/tetond/src/harness/budget.rs` — the one composer for the sentence
- `crates/teton/src/session_ui.rs` — `/policy show`, `/verbose` renderings
- `crates/teton/src/cli_rows.rs` — `/provider list` rows
- `crates/teton/src/client.rs` — `/doctor` output

## Acceptance Criteria

- [ ] The sentence reads `window 1,000,000 tokens; budget 665,984 words
      (≈1,000,000 tokens at 3/2)` on `kimi-k3`
- [ ] `/provider list`, `/policy show` and `/doctor` all print it (AC-2)
- [ ] It is composed in **one** place and read by every surface — no surface
      re-derives it (the `conventions.md` "compose the sentence where the facts
      are" rule, and BR-8's one-classifier-per-fact posture)
- [ ] The local route prints the same shape with `bound = local_engine`
- [ ] The word half is called *words* and the window *tokens*, everywhere

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-6 | test-case | `crates/tetond/src/harness/budget.rs::window_sentence_names_both_currencies` | no |
| AC-2 | test-case | `crates/teton/src/cli_rows.rs::provider_list_prints_window_and_budget` | no |

## Technical Notes

- LESSON-446 is the reason this task exists at all: two limits that constrain the
  same text must be stated in the same unit, or the boundary must own an explicit
  conversion. Here the conversion is stated in the sentence itself.

---
id: TASK-007
title: "A preamble fallback is reported, not silently folded"
status: draft
parent: REQ-615
created: 2026-09-04
updated: 2026-09-04
dependencies: [TASK-001, TASK-002]
---

## Description

BR-6 — and the `||` split without which the fact is unobservable (architecture
ADR-6).

## Files to Create/Modify

- `crates/tetond/src/skills/dynamic.rs` — the split in `run_one`, `fell_back` on
  `DynamicOutcome::Ran`.
- `crates/tetond/src/skills/expand.rs` — the harness prefix line at the fold.
- `crates/tetond/src/harness/tools/skill.rs` — publish `skill_preamble_fallback`.

## Acceptance Criteria

- [ ] `run_one` splits the command at its **first top-level `||`** — outside
      quotes, and not a single `|` — runs the primary, and runs the remainder
      only when the primary exits non-zero.
- [ ] `a || b || c` splits into primary `a` and remainder `b || c`; the remainder
      is handed to the shell whole, so a chain's semantics stay the shell's.
- [ ] A command with **no** top-level `||` runs byte-identically to today and can
      still report a failure by exiting non-zero.
- [ ] A `||` inside quotes is **not** a separator (`echo "a || b"` is one
      command) — the benign path.
- [ ] When the primary failed, the folded output is prefixed with
      *"[preamble &lt;n&gt; fell back: `<primary verb>` failed in <root>]"* and
      `skill_preamble_fallback` is published.
- [ ] When the primary succeeded, there is **no** prefix and **no** event.
- [ ] The event carries no output (TASK-001's shape).
- [ ] `cargo test -p tetond skills` passes.

## Verification

| rule | kind | artifact | benign_path |
|------|------|----------|-------------|
| BR-6 | test-case | `crates/tetond/src/skills/dynamic.rs::the_primary_of_a_fallback_command_is_run_and_observed` | yes |
| BR-6 | test-case | `crates/tetond/src/skills/dynamic.rs::a_quoted_or_absent_separator_changes_nothing` | yes |
| BR-6 | test-case | `crates/tetond/src/skills/expand.rs::a_fallback_is_prefixed_and_a_success_is_not` | yes |
| AC-5 | test-case | `crates/tetond/src/skills/expand.rs::a_fallback_is_prefixed_and_a_success_is_not` | yes |

## Technical Notes

Use TASK-002's `split_top_level` helper — one quote-aware scanner, used by the
redirection detector and by this split. Do not write a second one.

The prefix is written **where the fold renders the outcome**, not where the
command ran (LESSON-477: sanitize and frame at the authoring layer). The
`<primary verb>` in the line is the primary's first command-position word, which
is user-authored skill text — bound and defuse it at the frame it is spliced
into, exactly as the not-run placeholder already does with `Command::as_str`.

`fell_back` is a new field on `DynamicOutcome::Ran`. Every construction site must
set it; do not give it a `Default` (architecture.md: a required field with no
`Default` is how "every call states X" is enforced).

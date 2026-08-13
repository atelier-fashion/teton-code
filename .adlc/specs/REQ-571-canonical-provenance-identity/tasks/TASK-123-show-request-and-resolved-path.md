---
id: TASK-123
title: "Show both request and resolved path when they differ"
status: complete
parent: REQ-571
created: 2026-08-13
updated: 2026-08-13
dependencies: [TASK-119]
---

## Description

Implement BR-11. When a resolved path differs from the request string, `read`
and `edit` output must show both, so a model reading through a symlink or an
absolute path is not told it read something other than what it read.

## Files to Create/Modify

- `crates/tetond/src/harness/tools/read.rs` — output text shows request and resolved form on divergence.
- `crates/tetond/src/harness/tools/edit.rs` — same.
- `crates/tetond/tests/symlink_posture.rs` — add the AC-15 cases.

## Acceptance Criteria

- [x] AC-15: when request and resolved path differ, `read` output contains both.
- [x] AC-15: when they match — the overwhelmingly common case — output is byte-identical to today.
- [x] Display remains separate from provenance: only the `ProvenanceId` governs enforcement, and changing the displayed text cannot alter a boundary verdict.
- [x] `edit`'s success line carries the same treatment.
- [x] AC-9 regression: the six existing egress suites still pass.

## Technical Notes

Verified safe at spec time: nothing currently depends on the echoed string — no
test asserts on it, and `with_paths` has seven call sites, all within the four
tools and two egress tests. The byte-identical-when-matching criterion is what
keeps that true for the existing suites.

Keep the divergence rendering compact; this text enters model context on every
read, so a verbose form costs tokens on every turn.

## Implementation Notes (as landed)

- **The form is `` `request` -> `resolved` ``,** one line, no prose. In `read` it
  is prepended above the line-numbered window (there was no filename in that
  output at all before, which is *why* BR-11 exists: the only name the model held
  was the one from its own request, and that is the name that is wrong). In
  `edit` it replaces the file name in the existing success line — ``edited
  `notes.txt` -> `secrets/prod.env`: replaced 1 occurrence. …`` — and `read`'s
  empty-file note takes the same substitution. One shape for both tools, ~8 extra
  tokens, and only on the calls that have something to disclose.
- **A leading `./` is normalized away before comparing** (`strip_prefix("./")`,
  exactly once). `./x` and `x` are the same request in different words, and
  treating that as a divergence would print a second name on a large share of
  ordinary reads and train the reader to skim past the line that matters. Every
  other spelling is shown, *including* the neighbours `.//x` and `././x`: the rule
  is deliberately dumb, because it is a display heuristic and a cleverer one would
  be one more thing that can be wrong about a path. Pinned by
  `a_dot_slash_request_is_not_a_divergence`; deleting the strip fails it and the
  `edit` byte-identity test.
- **Byte-identity is asserted by exact comparison, never `contains`.** Three
  tests (`read` inline, `edit` inline, and one at the integration level) compare
  the whole `content` against the literal pre-BR-11 rendering — `"     1\talpha\n
  …"`, ``"edited `f.rs`: replaced 1 occurrence. Verify the change before
  finishing."``. `contains` is exactly what a stray note bolted on top would also
  satisfy, so it cannot make this claim.
- **Where the helper lives.** `shown_path` / `divergence_note` sit in `read.rs`
  and `edit.rs` imports the first (`use super::read::shown_path;`, the same
  sibling-import shape `grep.rs` already uses for `glob::glob_match`). `tools/mod.rs`
  next to `Resolved` is arguably the better long-term home; this task's footprint
  excluded it and two other agents were editing that tree concurrently. Moving it
  is a rename, not a redesign.
- **Display/provenance separation is checked, not asserted.** `with_paths` takes
  `ProvenanceId`s, so no string built for display can reach the matcher by
  construction (ADR-A) — the added test exercises the property at the real choke
  point instead: one file read twice, once through a link and once by its own
  name, renders *differently* (`assert_ne!` on content, so the equality below is
  not vacuous) and is judged *identically* — same minted id, same `PrivacyBlocked`
  path, one `privacy_block` each.
- **Mutation-checked, both halves.** Dropping the note from `read` fails three
  tests across both levels (`a_divergent_request_shows_both_forms`,
  `read_shows_both_the_link_and_the_target_it_resolved_to`,
  `divergent_display_cannot_move_the_boundary_verdict`); dropping the `./` strip
  fails two.
- **Nothing downstream parses this text.** `ReadTool`/`EditTool` `content` reaches
  only the model (the registry is their sole non-test caller), and the tools set
  no `measured`, so this does not repeat the "tool re-parsed its own prose" failure
  that field exists to prevent.

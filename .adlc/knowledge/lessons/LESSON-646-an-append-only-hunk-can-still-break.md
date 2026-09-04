---
id: LESSON-646
title: "The append-only bound can hold on a hunk whose keep-both resolution does not compile"
component: "adlc/conflict-bound"
domain: "adlc"
stack: ["git", "sh", "rust"]
concerns: ["reliability", "process"]
tags: ["merge-conflict", "keep-both", "append-only", "BUG-207", "rebase", "sprint"]
req: REQ-614
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-614's Phase-7 blocker recorded `partials/conflict-bound.sh`'s verdict:
three offenders (`shell.rs`, `runtime/turn.rs`, `runtime_visibility.rs`), and
by implication every other conflicted file — including
`harness/tools/mod.rs` — classified as an append-point collision, resolvable
under BUG-207's bound.

`mod.rs` was classified correctly and would still have been resolved wrong.
Two REQs each added a pair of methods before the same following method, so
every hunk's base section was genuinely empty. But git ended the conflict
region *mid-construct*: the body read `…&self.boundaries` / `…&self.known_projects`
with the shared `    }` sitting **after** `>>>>>>>` as common context.
Concatenating the two conflict bodies therefore closed only the second side's
method and left the first one open. The file did not compile.

The verification step would not have caught it. `adlc_conflict_verify_kept`
proves every line each side contributed is present in the result — and in the
broken file, every line *was* present. Line preservation and syntactic
validity are different properties.

## Lesson

"Every hunk's base section is empty" proves both sides only **added**. It does
not prove the conflict region is a whole syntactic unit, and a keep-both is
only safe when it is. When git's region ends mid-construct, the trailing common
lines belong to *both* sides and the earlier one must get its own copy.

So the bound needs a second, equally mechanical check: after a keep-both,
compile or parse the file. On this repo `cargo check --workspace --all-targets`
is the check, and it is the one that fails on an unclosed `impl`. Until a
resolver runs one, treat a passing append-only classification as "safe to
attempt", not "safe to ship".

## Why It Matters

This is the failure mode BUG-207's bound was written to exclude, arriving
through the one door the bound leaves open. An unattended `/sprint` runner
that trusted classification plus line-preservation would push a
non-compiling tip, and — because a conflicted PR gets no CI at all
(LESSON-643) — the first signal would come from a human reading a red build
on a branch everyone believed was mechanically resolved. Here the cost was one
`cargo check`; the cost of not noticing is a resolution nobody re-reads
because a helper said it was verified.

## Applies When

Resolving any merge or rebase conflict under a keep-both rule — BUG-207's
bounded path, `adlc_conflict_keep_both`, or a hand-rolled equivalent — in a
brace-, tag- or indentation-delimited language, especially when two sides
appended at the same point inside an existing block.

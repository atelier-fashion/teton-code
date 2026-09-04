---
id: LESSON-632
title: "Anchoring a new item on `fn name(` inserts it between the next item's doc comment and the item"
component: "adlc/implementation"
domain: "tooling"
stack: ["rust"]
concerns: ["maintainability", "developer-experience"]
tags: ["doc-comment", "insertion", "traceability", "rustdoc", "attribute", "scripted-edit"]
req: REQ-615
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

Twice in one REQ, a new item was inserted by anchoring a scripted edit on the
target's `fn`/`pub fn` line. In Rust the doc comment and any attributes sit
*above* that line, so both insertions landed **inside** the neighbour's header:

- in `skills/mod.rs` a new method landed between `invocable_by_model`'s
  `#[must_use]` and its `fn`, which `rustc` reported as an unused attribute;
- in `runtime/turn.rs` a helper landed between `settle_dynamic_context`'s
  40-line doc block and its `fn`, silently transplanting twelve rationale ids
  (`ADR-6`, `ADR-14`, `BR-12`, `LESSON-495`, …) onto a function that has nothing
  to do with any of them.

The first was a compiler warning. The second compiled, ran, and passed every
behavioural test; `traceability_sweep` caught it by noticing that every id had
"left" the item it explained.

## Lesson

An anchor for inserting a Rust item is the **start of the target's doc block**,
not its signature line. Walk up from the signature past every contiguous `///`
and `#[…]` line first, and insert above that. When a scripted edit adds an item
near another, verify placement by reading the result rather than by building —
the compiler is silent on the case that matters, because a doc comment attached
to the wrong item is still a valid doc comment.

## Why It Matters

Rationale is this codebase's main defence against a later edit undoing a
deliberate decision, and a doc block attached to the wrong function is worse
than a missing one: it reads as an explanation of code it does not describe.
Nothing in the type system, the tests, or clippy sees it. Only a sweep that
tracks which item each id annotates does — which is why that sweep exists, and
why it earned its keep twice in one REQ.

## Applies When

Any scripted or tool-driven insertion of a Rust item (`fn`, `struct`, `const`,
`impl`) adjacent to an existing one — and generally in any language where
documentation or attributes precede the thing they describe.

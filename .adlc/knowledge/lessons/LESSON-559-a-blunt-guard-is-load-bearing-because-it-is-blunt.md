---
id: LESSON-559
title: "A deliberately blunt architectural guard should be routed around, not narrowed — the bluntness is the guarantee"
component: "architecture"
domain: "verification"
stack: ["rust"]
concerns: ["architectural-guards", "source-scanning-tests"]
tags: ["guard", "source-scan", "re-export", "narrowing", "req-588", "req-558"]
req: REQ-588
created: 2026-08-23
updated: 2026-08-23
---

## What happened

`cost/ledger.rs` holds the only text→category map in the daemon. A source-scanning
test asserts the file cannot name `teton_core` **at all** — not
`teton_core::Category` specifically, the whole crate — so the routing type can
never arrive there under an alias, a re-export, or a future rename.

Threading REQ-588's spend accumulator meant importing
`teton_core::cost_ceiling::PromptSpend` into that file. The guard fired. The
import was completely innocent: a different item, in a different module, with no
path to a `Category`.

The tempting fix was to narrow the guard to `teton_core::Category`.

## The lesson

**Narrowing a blunt guard to accommodate an innocent case destroys exactly the
property it was protecting.** The guard is not a substring check that happens to
be crude; the crudeness *is* the guarantee. Once it matches only the literal
`teton_core::Category`, an alias, a `use ... as`, or a re-export defeats it — and
the next person to trip it will narrow it again, for reasons equally innocent.

The right move is to route around: a `pub(crate) use` in the parent module, and
the ledger imports `super::PromptSpend`. One line, guard intact, and the comment
on the re-export says why it exists so nobody "simplifies" it back.

## How to apply

- When a source-scanning guard fires on something innocent, first ask whether the
  guard's breadth is deliberate. If its comment says "at all", it is.
- Prefer a re-export, a newtype, or a module boundary over relaxing the assertion.
  Changing the code is reversible; a weakened guard fails silently forever.
- If you genuinely must relax one, the change belongs in its own commit with the
  threat model restated — not folded into a feature branch as a one-line edit.

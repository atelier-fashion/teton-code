---
id: LESSON-486
title: "A doc comment is an assertion nothing runs"
component: "docs"
domain: "maintainability"
stack: ["rust"]
concerns: ["maintainability", "correctness"]
tags: ["doc-comments", "drift", "false-claims", "deletion-hygiene"]
req: REQ-558
---

## What Happened

Three separate sessions, working on the same crate within two days, each hit a
variant of one defect: **a comment that outlived, wandered from, or lied about its
subject.**

1. **REQ-557** deleted `billing_model` and left its doc comment behind, orphaned
   above an unrelated function. Only `clippy::empty_line_after_doc_comments`
   caught it — no human review did, across a full multi-agent panel.
2. **REQ-558** shipped a comment on a hand-written `Deserialize` claiming it bought
   a good error message *"for every format at once, config TOML and the JSON-RPC
   payload that binds a category at runtime alike."* It does not: `config/set`
   deserializes the **protocol** type, which derives `Deserialize`. The task note
   had correctly hedged "any *future* JSON-RPC payload"; the comment promoted that
   to present fact.
3. **REQ-558** also shipped a doc comment asserting that a specific mutation turned
   a test red. It had not been run. When it was, the suite stayed green.
4. **PR #59**, from an unrelated session, restored `validate_local_model`'s rustdoc
   after it had drifted onto a different function entirely — in `config.rs`, the
   same file as #1.

## Lesson

Code has a compiler and tests. Prose has neither. A comment is an assertion that
nothing will ever check, in the one place a future reader is most inclined to
believe without checking — which makes an inaccurate comment strictly more harmful
than no comment.

Three rules that would have caught all four:

**Delete a comment in the same change as its subject.** A deletion "obviously"
implied by another task is a deletion nobody performs. Name the owner explicitly —
REQ-558's ADR-J had to exist purely because nothing owned `policy::evaluate`'s
removal, and its 150-line test module would otherwise have survived as phantom
coverage.

**A claim about behaviour goes in a test, not a comment.** "This mutation turns X
red", "this is rejected on every path", "this cannot happen" — each is a testable
proposition. Write the test and let the comment point at it. #3 above is the pure
case: the comment asserted a verification that was never performed.

**Say what you verified, not what you expect.** "Any *future* JSON-RPC payload" was
accurate; "the JSON-RPC payload alike" was not. The distance between them is one
tense, and it is the distance between a note and a falsehood.

## Why It Matters

The failure is silent and compounding. Nothing fails, nothing warns, and the comment
accrues authority with age — by the time it is wrong enough to matter, it reads as
established fact and the code around it has been written to agree.

It also survives review. All four instances passed multi-agent panels: reviewers
read a comment as context for the code rather than as a claim to check. One was
caught by clippy, one by a mutation run, one by an adversarial pass explicitly told
to hunt false claims, and one by a session that happened to be reading that file for
another reason.

## Applies When

- Deleting a function, type, variant, or module. Grep for its name in prose,
  including doc links (`[`Foo`]`), before considering it gone.
- Writing a comment containing "always", "never", "on every path", "cannot", or a
  claim that a check or mutation was performed. Each is a test.
- Moving code. A rustdoc attaches to whatever follows it; a reordering silently
  re-parents it. #4 was exactly this.
- Reviewing. Read comments as claims to verify, not as background. A wrong comment
  is a defect with no failing test attached.

Related: [[LESSON-484]] (enforce the rule where the decision is made), [[LESSON-485]]
(a fixture that cannot discriminate), [[LESSON-443]] (guard conditions that disable
themselves).

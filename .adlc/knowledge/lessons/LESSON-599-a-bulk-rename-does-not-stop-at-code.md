---
id: LESSON-599
title: "A bulk identifier rename does not stop at code — it rewrites strings the user reads and comments the reviewer trusts"
component: "daemon/session"
domain: "refactoring"
stack: ["rust"]
concerns: ["reliability", "developer-experience"]
tags: ["refactor", "mechanical-edit", "string-literals", "comments", "review"]
req: REQ-600
created: 2026-09-01
updated: 2026-09-01
---

## What Happened

REQ-600 extracted stages out of a 1,084-line function. Each extraction moved
locals into a parameter bundle, so every use had to be re-spelled —
`attempts` → `st.attempts`, `edited` → `latches.edited`, and so on. That is a
word-boundary regex over the extracted body, and it was applied that way.

It rewrote a string literal.

```rust
"You latches.edited a file but have not latches.verified the change. Run a \
 verification step (re-read the file, or run a build/test with the shell tool) \
 and confirm the result before finishing."
```

That is BR-6's mandatory-verification nudge — text pushed into the model's
context and visible in a user's transcript. It fires under the `Degraded`
harness profile, which exists **for weak models**: the reader least able to
parse `latches.edited` as "edited" is the only reader who ever sees it.

The compiler was silent, `cargo clippy` was silent, and 4,072 tests were silent,
because no test asserts on that sentence. The commit that shipped it says
"bodies are byte-identical".

It also rewrote thirteen comment sentences: "the consent `tctx.gate` asks the
user", "the budget follows the `st.route`", "a human was shown and
`st.accepted`", "a credential that will not resolve is a `tctx.core.config`
problem". In a codebase where the reason for an ordering lives in the comment
above it, that is not cosmetic damage — and `traceability_sweep` could not see
it, because the REQ ids were all still present and still adjacent to their
subject.

Two independent reviewers found the string literal. Neither found it by reading
the diff for correctness; both found it by **diffing the production text against
`origin/main` and asking what changed that should not have**.

## Lesson

**A rename applied to a region applies to the whole region. Code is the part you
were thinking about; strings, comments and doc text are the rest of it.**

- **Bound mechanical renames to code tokens.** Skip string and char literals and
  comment bodies. A regex over raw text does not know the difference, and the
  one place the difference is invisible to every automated check is the one
  place it reaches a user.
- **A silent test suite is not evidence here.** These edits change bytes that
  nothing asserts on. The absence of a failure carries no information, which is
  the opposite of the usual case and is easy to misread as safety.
- **Diff the prose, not just the code.** `git diff origin/main..HEAD | grep
  '^[-+].*//'` over a relocation surfaces the whole class in one pass, and a
  refactor that claims "bodies are byte-identical" should be able to survive it.
- **Model-facing strings deserve a test.** Not because they are likely to
  change, but because when one does change nothing else will say so. The nudge
  above still has no test; that is recorded rather than fixed.

## Why It Matters

The failure is invisible in exactly proportion to how much you trust the tooling.
A rename is the safest edit there is — right up to the moment its scope is a
block of text rather than a syntax tree — and "the suite is green and the
compiler is happy" is the strongest possible signal, on a change where both are
structurally incapable of noticing.

And the cost lands on the user, not the developer: a weak model gets an
instruction it cannot parse, in the one situation the daemon has decided it
needs extra help.

## Applies When

Any bulk find-and-replace over a code region — extraction, parameter bundling,
a type rename, a module move. Any refactor whose commit message contains the
phrase "byte-identical". And reviewing one: read the removed lines, not only the
added ones, and pay particular attention to strings and comments, which are
where an automated check will not meet you.

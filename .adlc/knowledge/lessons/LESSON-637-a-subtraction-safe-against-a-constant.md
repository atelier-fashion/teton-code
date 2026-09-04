---
id: LESSON-637
title: "A subtraction that is safe against a constant is not safe against a runtime value"
component: "daemon/harness"
domain: "inference"
stack: ["rust"]
concerns: ["correctness", "reliability", "security"]
tags: ["const-fn", "underflow", "runtime-window", "input-validation", "saturating"]
req: REQ-616
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-616 turned several compile-time byte caps into functions of the engine's
window, so that a 262,144-token engine would size its scan and compaction budgets
to what it was actually serving:

```rust
// before: const, evaluated once against 32_768
pub const COMPACT_PROMPT_BUDGET_BYTES: usize =
    (LOCAL_ENGINE_N_CTX as usize - COMPACT_DUTY.max_tokens() as usize) * …;

// after: const fn, evaluated against whatever the loader decided
pub const fn compact_prompt_budget_bytes(n_ctx: u32) -> usize {
    (n_ctx as usize - COMPACT_DUTY.max_tokens() as usize) * …
}
```

The body was copied across unchanged, and that was the defect. `32_768 - 4_096`
is provably fine and had been for the life of the constant. `n_ctx - 4_096` is
fine only for `n_ctx > 4_096`, and the same REQ had just added `[inference]
n_ctx` — a number a user types, with no lower bound. `n_ctx = 512` passed
validation, reached the subtraction, and **wrapped to roughly 18 exabytes of
prompt budget in release** (it panics in debug, which is how it was found). A
compactor believing it had that much room would have sent the engine everything
it could reach.

## Lesson

**Constant-folding a parameter changes the proof obligations of every expression
that reads it.** When a `const X = f(SOME_CONST)` becomes `const fn f(runtime)`,
each arithmetic operation in `f` silently loses the compile-time guarantee it was
written under. Re-derive them; do not port the body.

Two guards, not one, and for different reasons. The **validation** keeps the bad
value out and is what the user sees; the **saturating arithmetic** is what holds
when some later caller reaches the function by a route the validation does not
cover (a different config path, a test fixture, a default that changes). A single
guard here is the "one enforcement point for an invariant with several" shape.

Prefer a floor derived from the code rather than chosen: this one reads
`DUTY_MAX_TOKENS_REQUEST`, so a duty that raises its reservation raises the floor
with it, and nobody has to remember.

## Why It Matters

The release behaviour is the dangerous one, and it is the one no test run in
debug will ever show you. `cargo test` panics and tells you; a shipped binary
computes a nonsensical budget and proceeds confidently. A guard that only exists
in `debug_assertions` is not a guard.

The general shape — a value moving from compile-time to runtime — is common and
comfortable-looking, because the diff is small and the arithmetic is unchanged.
The arithmetic being unchanged is exactly the problem.

## Applies When

Converting a constant to a function parameter; adding a user-settable value that
feeds existing arithmetic; reviewing a `const` → `const fn` refactor; any
subtraction or division whose operands stopped being literals.

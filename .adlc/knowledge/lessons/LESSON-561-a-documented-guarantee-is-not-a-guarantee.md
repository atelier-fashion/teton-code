---
id: LESSON-561
title: "Audit the sentences that claim a guarantee as hard as the code that provides one"
component: "daemon/harness"
domain: "harness"
stack: ["rust"]
concerns: ["security", "reliability"]
tags: ["doc-comments", "false-invariants", "code-review", "cfg-test"]
req: REQ-591
created: 2026-08-25
updated: 2026-08-25
---

## What Happened

A five-agent panel attacked REQ-591's trust gate — a security feature that deliberately widens
authority. It could not construct a bypass of any part of the security core: the invoker-scoped
row derivation, the `NoTerminal`-only settlement rewrite, exact-match consultation, and the
identity taken from the snapshot the bodies were read under all held. `cargo audit` was clean.

**Every finding was somewhere the implementation asserted a property instead of building one:**

- `install_commitment_attestation` was `OnceLock::set` — first-writer-wins. Its own doc said the
  slot was *"replaceable, which that one is not… a first-writer-wins slot would leave an injected
  verifier inert on exactly the paths a fixture installed it to exercise."* The re-wire it
  described was a no-op. Four of five reviewers found it independently.
- A doc claimed a TOCTOU window was *"not narrowed, it is closed."* Two resolutions remained; the
  window was sub-millisecond, not absent.
- A doc claimed both rendered roots were home-relative *"so no username reaches the line"* — true
  before an earlier decision made one of them absolute, false after.
- `#[cfg(test)]` on a resolve-then-name helper was described as an architectural guard against
  the shape returning. The sibling function was `pub(crate)` over any `&Path`, so the shape went
  back into production in two lines without touching the marked one. One test held the property.
- A test's comment described a newline guard its assertion could not see: the assertion counted
  lines containing a needle that only ever appears in the first half of a split.

## Lesson

Read doc comments as claims to be falsified, not as documentation. When a comment states an
invariant, find the line that enforces it or delete the sentence. Two specific shapes recur:
a comment naming the failure mode it prevents (check that it does), and `#[cfg(test)]` or
visibility described as a guard (it removes one *spelling* of a mistake, not the mistake).

## Why It Matters

A false invariant is worse than a missing one. The next reviewer trusts it and stops checking;
the next fixture is written against a seam that is inert and goes green for the wrong reason. In
this case a test injecting a *refusing* verifier would have passed while silently consulting the
shipped one — certifying a control it never touched.

## Applies When

- Reviewing security code, permission gates, or anything with an injection seam for tests.
- Any comment containing "cannot", "closed", "always", "never", or "replaceable".
- A decision changed a value's shape (absolute vs relative, owned vs borrowed) — every sentence
  describing the old shape is now a candidate falsehood.

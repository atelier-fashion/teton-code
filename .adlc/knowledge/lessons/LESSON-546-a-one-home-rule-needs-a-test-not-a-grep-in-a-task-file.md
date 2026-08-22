---
id: LESSON-546
title: "A one-home-per-fact rule enforced by a grep in a task file is not a guard — the next copy ships green"
component: "adlc/process"
domain: "harness"
stack: ["rust"]
concerns: ["developer-experience", "reliability"]
tags: ["one-home", "lesson-456", "duplication", "mutation-testing", "process", "req-586"]
req: REQ-586
created: 2026-08-20
updated: 2026-08-20
---

## What Happened

REQ-586 derives a chain of constants (context budget → body size → scan cap →
prompt → engine window) and leaned hard on LESSON-456: one home per fact. Its
verification task carried a checklist item — `grep -rn "89_127\|89127"` must
be empty, `4_096` and `1_500` must each have exactly one non-test home — and
the task's mutation table honestly recorded that replacing the derived
`REDACT_SCANNABLE_CONTEXT_BYTES` with the literal `89_127` **passes every
assertion**, with the grep as the only detector.

The sweep found three genuine second homes and fixed them, including the
scannable bound copied into a *different crate* (`crates/teton/src/session_ui.rs`,
which cannot even see the constant). The instances were fixed. The class was
not: the detector runs when a human remembers to run it, and the task file
that records it is an artifact nobody reads again after the REQ closes.

A later reviewer put it plainly: the next copy of `89,127` or `4,096` ships
green.

## Lesson

**If a rule is worth enforcing, it is worth a test.** A grep in a task file is
documentation of an intention, not a guard — it has no schedule, no owner, and
no failure mode. The same rule as a `tests/one_home.rs` that walks the crate,
skips `#[cfg(test)]` sections, and asserts each pinned literal appears once is
~40 lines and turns "somebody should check" into a red build.

The corollary is about honesty in mutation tables: a mutation recorded as
"expected to pass, the grep is the catch" is a confession that the class is
unguarded. That is the right thing to write down — and it should be read as a
work item, not as coverage.

## Why It Matters

A derived-constant chain is only as good as the rule that keeps it single. The
moment a number acquires a second home, the two drift on different schedules
and the failure surfaces far away — a budget that no longer matches the cap it
was derived from, discovered as a blocked turn on somebody's machine. The
whole point of deriving is to make that impossible; a literal copy quietly
re-opens it, and the suite says nothing.

## Applies When

Any REQ that derives constants from each other (budgets, caps, chunk sizes,
timeouts); any verification task whose checklist contains a `grep`; any
mutation table with a row recorded as "expected to pass". Turn the grep into a
test in the same PR, while the list of pinned literals is still in someone's
head.

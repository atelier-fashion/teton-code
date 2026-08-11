---
id: LESSON-502
title: "An invariant enforced at several seams needs an adversarial test at each seam — a passing suite proves only the seams you wrote tests for"
component: "daemon/session"
domain: "privacy"
stack: ["rust", "daemon"]
concerns: ["security"]
tags: ["mutation-testing", "verification-gaps", "authorization", "cross-call-site", "monitor", "req-568"]
req: REQ-568
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

REQ-568's "a `monitor` connection may *watch* every session but *drive* none"
invariant is enforced at two seams: `handle_session_clear` (through `dispatch`)
and `spawn_prompt_turn` (which bypasses `dispatch`). Both call `conn.may_drive`,
and `may_drive` is deliberately distinct from `may_receive` — the whole point is
that sight (monitor) must not confer drive. The implementing task (TASK-099)
mutation-verified its gates *in isolation*: inverting the clear gate failed the
clear tests, inverting the prompt gate failed the prompt tests. All 1989 tests
passed, twice over.

But the *monitor* case had a witness only at the clear seam
(`a_monitor_may_watch_every_session_and_drive_none` drives clear). No test ever
issued a `session/prompt` from a monitor connection. So swapping the prompt
seam's `may_drive` for `may_receive` — the exact bug the code's own doc comments
warn against — left every test green while silently promoting every observer
into a driver of every session it could see. The self-run per-task mutation
checks could not catch it: they mutate the code a task touched and re-run that
task's tests, and no task's tests exercised prompt-from-a-monitor. Only the
verify phase's adversarial test-coverage audit, tracing the *conceptual*
invariant to each of its enforcement sites, found the hole.

## Lesson

A security invariant enforced at N seams is N independent claims, and a green
suite only certifies the seams that have a test which would fail if that seam's
check were removed or weakened. Per-task mutation verification is scoped to the
task's own files and tests — it is structurally blind to a sibling call site
that enforces the same rule. When you write `may_drive` in two places, write the
"can't drive" adversarial test in two places. Prefer collapsing the seams (route
every mutating method through one gated `dispatch`) so there is only one site to
test; when a method must bypass the common path (as `spawn_prompt_turn` does for
its own-task concurrency reason), that bypass is exactly where the missing test
hides. This is the multi-call-site generalization of [[LESSON-479]]: an
invariant is only tested in the direction — and at the site — your test actually
exercises.

## Why It Matters

The missed swap is a full cross-session authorization bypass, not a cosmetic
gap, and it survived the strongest signal the implementer had (self mutation
checks + a near-2000-test green suite). The same audit pass found a Critical
([[BUG-161]], permission `request_id` collision) and the ungated
`permission/respond`/`session/list` surfaces now tracked in REQ-569 — all
"edges the fix drew" that a passing suite hid. A green suite is evidence about
the tests you wrote, never about the invariant you meant.

## Applies When

Reviewing or implementing any capability check that appears at more than one
call site (dispatch handler + spawned task, middleware + direct API, CLI + RPC);
deciding whether a task's own mutation checks are sufficient evidence; auditing
test coverage in the verify phase — enumerate the invariant's enforcement sites
and confirm each has a test that goes red when that site's check is inverted.

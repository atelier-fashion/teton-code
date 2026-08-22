---
id: LESSON-547
title: "Two tasks that each say the other owns a rule leave it owned by nobody — and the narrow check looks like the check"
component: "adlc/process"
domain: "clients"
stack: ["rust"]
concerns: ["correctness", "security"]
tags: ["parallel-implementation", "ownership", "reserved-names", "seam", "task-decomposition", "req-585"]
req: REQ-585
created: 2026-08-20
updated: 2026-08-20
---

## What Happened

REQ-585 splits a name contest across a seam. The daemon builds the skill
registry and marks what it can see shadowed; the client owns `COMMANDS` and
decides what a `/` line dispatches to. Two task files wrote down who owns the
**reserved-name** half, and they wrote down opposite answers.

TASK-195's `skills::mod` says the reserved case belongs to the client, with the
right reason — `tetond` has no `COMMANDS` to read. TASK-206's `reserved_names`
doc said the daemon marks such a skill shadowed. Each implementer had read the
other's task file and reasonably concluded the rule was already handled.

Nobody handled it, and the shape of the bug kept it quiet: **the narrow check
looks like the check.** No row is spelled bare `provider`, so
`builtin_row("provider")` answers `None` and an unshadowed skill named
`provider` took `/provider foo` while `/provider list` stayed with the table —
one spelling reaching two handlers, which is what REQ-555 forbids. `/teton` was
listed unmarked for the same reason.

It was found in review, not by a test, because the tests were built on fixtures
where reserved names arrived **pre-marked** — a shape the daemon never sends.

The second half surfaced in the same REQ's verify pass. Once the client's half
was fixed, the *daemon* still had no check at all: `SkillRegistry::dispatchable`
answered for a skill named `cost`, so any client not carrying `teton`'s table —
the phase-2 one, or a third party — could dispatch a repo-supplied
`.claude/skills/cost/SKILL.md` by name. ADR-1's own rule is that every rule with
teeth lives in the daemon; this one had none there.

## Lesson

**A rule that crosses a seam is owned by exactly one side, and the other side's
task file must name that side rather than assume it.** Two documents that each
gesture at the other are indistinguishable, at review time, from two documents
that agree — and the gap between them has no compiler, no test, and no reviewer
assigned to it.

Three habits close it.

**Write ownership as a pointer, not a description.** "The client decides this"
and "the daemon marks it" are both descriptions. "TASK-206 owns this; this
module must not re-decide it" is a pointer, and a pointer at a task that does
not claim the rule is a visible contradiction.

**Derive the rule from one source and test the derivation in both directions.**
The fix makes `table_claim` derive the whole reserved set from `COMMANDS` —
rows, aliases, the first word of every multi-word row, and `teton` — with
`classify` and `/help` reading it, so `/help` cannot promise what `classify`
declines. When the daemon needed the same set, it got a list in
`teton-protocol` and a test in `teton` asserting the list *is* the derivation.
Writing that cross-check immediately caught two things the author had guessed
wrong: multi-word rows can never collide with a skill name (the grammar forbids
the space), and six hand-listed names were claimed by no row.

**Suspect the check that is a special case of a wider rule.** "The name matches
a row" is a subset of "the table claims this name". Wherever the narrow check
is easy to write and the wide one takes a derivation, the narrow one is what
ships — and it passes every test written against names the narrow check covers.

## Why It Matters

The gap is security-adjacent: a file dropped in `~/.claude`, or committed to a
repo someone cloned, silently took a built-in command's argument form. And it
is the failure mode parallel task decomposition is *most* prone to — the more
carefully each task file reasons about what it does not own, the more
convincing the mutual deferral reads.

## Applies When

Any REQ whose tasks split across a client/daemon or module boundary and whose
rules are not all on one side; any place a reserved set, a precedence order, or
a shadow mark is computed in one crate and enforced in another. Ask, per rule:
which task file *claims* it, and does that file exist? Then ask whether the
crate that enforces it can actually see the source of truth — and if it cannot,
that is the design question, not an implementation detail.

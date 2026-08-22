---
id: BUG-185
title: "One consent buys an unbounded number of dynamic commands, and the invocation has no deadline of its own"
status: open
severity: medium
created: 2026-08-20
updated: 2026-08-20
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "security"]
tags: ["dynamic-context", "denial-of-service", "spawn_blocking", "timeout", "consent", "req-585", "req-560"]
---

## Description

Nothing caps the number of `` !`…` `` slots a skill body may hold, and there is
no whole-invocation deadline.

`skills::run_all` runs every command sequentially, each with its own
`DEFAULT_TIMEOUT_MS` (30 s), inside one `spawn_blocking`. A body can hold
thousands of slots, so **one** approved consent — or one `/name` at `full`,
which is the documented automation posture — buys hours of wall time on a
blocking-pool thread.

`spawn_blocking` work is not cancellable, so the connection-teardown path
aborts the *await* and leaves the closure running: the session stays claimed
(`SESSION_BUSY` for every later prompt) and the `ActivityGuard(Turn)` keeps the
daemon from idling out.

A related surface: `PermissionSubject::SkillDynamicContext.commands` carries
every command verbatim with no cap on count. Verbatim is correct — a bounded
consent that then ran the full command would be worse — but a hostile project
skill can list 400 innocuous commands with the dangerous one buried, rendered
one `Surface::line` each with no pagination. `/verbose`'s copy *is* bounded
(`COMMAND_ECHO_MAX_CHARS`), so the record the user checks afterwards and the
question they answered are different lengths.

## Impact

A cloned repo's `.claude/skills/*/SKILL.md` can wedge a session and hold a
blocking-pool thread, at `full` with no prompt at all. Denial of service, not
disclosure.

## Suggested fix

Cap the slot count at discovery with a named `SkipReason`, in the shape
`MAX_ENTRIES_PER_ROOT` already uses, and put a whole-invocation deadline around
`run_all` in addition to the per-command one. A slot cap also mostly closes the
consent-flooding surface; if it does not land, make the header's count
prominent and consider refusing outright above a threshold rather than
rendering a screen the user cannot read.

## Found

REQ-585 Phase 5 verify (security audit), 2026-08-20.

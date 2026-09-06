---
id: ASSUME-046
title: "None of the thirteen builtin boundary globs can name a skill file, so a user skill routes remote by default"
status: validated
req: REQ-619
created: 2026-09-05
resolved: 2026-09-05
---

## Assumption

That `DEFAULT_BOUNDARIES` (`**/.env`, `**/.ssh/**`, `**/*.pem`, … thirteen
globs) match no path of the form `~/.claude/skills/<name>/SKILL.md` or
`~/.claude/commands/<name>.md`, so a user skill carrying its `~`-scoped
identity reaches the wire unless the user writes a glob that names it.

## Context

REQ-619 BR-3 rests on this: the whole point of giving a user skill an identity
is that the builtins do not fire on it. If a builtin such as `**/.claude/**`
existed, the fix would have made every user skill pin **permanently** instead
of liftably — worse than the bug.

## Resolution

Validated, twice. At spec validation the shipped list was read against the
two path shapes; at implementation
`teton-core/src/boundary.rs::tests::a_tilde_scoped_id_is_matched_by_a_user_glob_and_by_no_builtin`
asserts it against the live `DEFAULT_BOUNDARIES` (mutation: swapping one
shipped glob for `**/.claude/**` reddens it). Adding a builtin that covers
`.claude/` is now a test-visible decision, not a silent one.

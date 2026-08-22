---
id: BUG-191
title: "AC-6 and AC-14 claim a pty leg for the acknowledgment prompt bytes; the pty suite has none"
status: resolved
severity: low
created: 2026-08-22
updated: 2026-08-22
component: "cli"
domain: "harness"
stack: ["rust", "cli"]
concerns: ["reliability", "developer-experience"]
tags: ["skills", "consent", "pty", "test-coverage", "req-587-residual"]
---

## Description

REQ-587 AC-6's evidence clause reads "(daemon unit + **pty for the prompt bytes**
+ `cli_e2e` for the pipe)", and AC-14 states "the pty suite covers **only** the
acknowledgment prompt bytes". `crates/teton/tests/pty_e2e.rs` is byte-unchanged
from `main` across the whole REQ, and its one skill test is REQ-585's, over the
*dynamic-context* consent rather than BR-4's acknowledgment.

## Reproduction Steps

1. `git diff origin/main...<REQ-587 branch> -- crates/teton/tests/pty_e2e.rs` — empty.
2. `grep -i acknowledg crates/teton/tests/pty_e2e.rs` — no match.

## Expected Behavior

A pty leg drawing BR-4's acknowledgment prompt at a terminal and asserting its
bytes: the root, one line per entry with the shadowing mark, and `+N more` only
when the daemon left some out.

## Actual Behavior

The prompt bytes are pinned only at renderer-unit level in `session_ui.rs`. The
`cli_e2e` leg that does exist asserts the **refusal without a terminal**, which
is the opposite claim.

## Root Cause

TASK-222's file list named `pty_e2e.rs` and it was never touched. The gap was
disclosed in one commit body but not in the task file or the spec — corrected at
wrapup, and filed here so it is trackable independently of REQ-587's spec.

## Resolution

Add the pty leg. The renderer-level assertions in `session_ui.rs` name the exact
bytes to expect.

## Files Changed

- `crates/teton/tests/pty_e2e.rs` — the leg to add
- `crates/teton/src/session_ui.rs` — where the bytes are pinned today
- Recorded in `.adlc/specs/REQ-587-model-invoked-skills/requirement.md` Deferred

## Closed — 2026-08-22

Closed by adding the pty leg. BR-4's acknowledgment is raised from `SkillTool::invoke` — the model's path — so the fixture uses the scripted local engine's text tool-call form. 22 project skills against `MAX_LISTED_PROJECT_SKILLS` (20), one shadowing a user skill of the same name, draws the root sentence, the shadowing mark, a bare-name entry and the `+2 more` tail in one prompt.

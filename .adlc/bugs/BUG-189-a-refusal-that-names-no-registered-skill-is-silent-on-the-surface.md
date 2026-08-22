---
id: BUG-189
title: "Two refusal reasons publish no record, so the session surface never says why"
status: open
severity: low
created: 2026-08-22
updated: 2026-08-22
component: "cli"
domain: "harness"
stack: ["rust", "daemon", "cli", "json-rpc"]
concerns: ["developer-experience", "reliability"]
tags: ["skills", "skill-tool", "refusal", "skill-invoked", "verbose", "req-587-residual"]
---

## Description

REQ-587 BR-9 says a refusal is never silent: one line per invocation, one line
per typed refusal. Five of the daemon's seven refusal reasons publish a
`skill_invoked` record carrying `refused: <reason>`, and the client renders a
`refused: skill …` line for each.

Two do not, so the model is told why and the human is not.

## Reproduction Steps

1. Have the model call `skill { name: "does-not-exist" }`.
2. Watch the session surface.

## Expected Behavior

A line naming the reason, as every other typed refusal produces.

## Actual Behavior

Only a `skill <name> [failed]` tool-call line. The reason reaches the model as a
typed result but never reaches the session.

## Root Cause

`SkillTool::refuse` publishes only when `registered_row` resolves a row. Two
cases have no row and therefore no file to describe:

- `unknown_skill` — no registry row carries that name;
- `invalid_arguments` — the parse is what failed, so `call_name` yields `None`
  (a capped **listing** call is the same shape: it named nothing).

`SkillInvoked` describes a skill **file**: a `source` (a closed two-variant
enum), a `path_display` and a `body_bytes`. Publishing one here would mean
choosing a root the file was never found under and inventing every identifying
field but the model's own spelling — a hollow record that reads like a real one,
which on the session surface is worse than the failed-tool line.

## Resolution

Closing it means a record whose subject is a **name** rather than a file — a
second event, or an optional `source` on this one. That is a protocol change
REQ-587 did not propose.

## Files Changed

- `crates/tetond/src/harness/tools/skill.rs` — `refuse`, `registered_row`
- Pinned:
  `crates/tetond/tests/skill_tool_loop.rs::every_tool_raised_refusal_over_a_registered_skill_publishes_a_record`
  and `…::a_run_of_listings_exhausts_the_per_turn_cap`
- Recorded in `.adlc/specs/REQ-587-model-invoked-skills/requirement.md` Deferred

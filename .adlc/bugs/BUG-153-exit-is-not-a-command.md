---
id: BUG-153
title: "/exit is not a command, so asking to leave gets an answer instead of an exit"
status: resolved
severity: low
created: 2026-08-04
updated: 2026-08-04
component: "cli/slash"
domain: "harness"
stack: ["rust", "cli"]
concerns: ["developer-experience"]
tags: ["slash-commands", "alias", "quit", "misattribution"]
---

## Description

`/exit` is what most REPLs call `/quit`, and it is what a user typed to leave a
session. Teton has no such command: on v0.1.7 it renders `unknown command:
/exit`, and on v0.1.6 — before the slash table existed — it went to the model,
which replied *"I understand you want to exit. I'll stop assisting with any
further requests. Is there anything else I can help you with before we part
ways?"* and did not exit.

That reply is the misattribution shape BUG-146 was about, one layer up: the user
addressed the harness and something else answered. It also spends a model call
and a turn of context on a line that was never a prompt.

## Reproduction Steps

1. Run `teton`.
2. Type `/exit`.

## Expected Behavior

The session ends, exactly as `/quit` and Ctrl-D end it — session-end cost
summary, exit 0, and nothing said about the exit itself.

## Actual Behavior

- v0.1.7: `error: unknown command: /exit — type /help for the commands this
  session knows.`
- v0.1.6: the line reaches the model, which answers conversationally; the
  session stays open.

## Environment

- Platform: macOS 26 / Apple Silicon (M5 Max, 48 GiB)
- Version: teton 0.1.6 (observed) and 0.1.7 (the unknown-command form)

## Root Cause

`COMMANDS` maps exactly one spelling to each row and has no notion of an alias,
so a second name for a command could only have been a second row — which would
have meant a second `/help` entry and a second handler to keep in step with the
first.

## Resolution

- `CommandSpec` gains an `aliases` field. `split_name` matches aliases
  alongside the canonical name and returns `CommandSpec::name`, so an alias
  canonicalises *before* anything looks it up: `/exit` cannot dispatch
  differently from `/quit` because by the time either reaches `resolve` it **is**
  `/quit`. No second row, no second handler, no second exit path (BR-6 holds
  unchanged).
- The longest-match key comes from the spelling that matched rather than from
  the row's canonical name, so a one-word alias cannot win a line belonging to a
  two-word row.
- `/help` renders aliases from the same rows it renders commands from, so BR-7
  covers them: a spelling that dispatches cannot be missing from `/help`.
- `/quit` is the only row with an alias, and deliberately so — a command worth a
  second name is worth a table row. The table is still six commands, which is
  what `the_table_carries_every_command_this_req_promises` counts.

## Files Changed

- `crates/teton/src/slash.rs` — `CommandSpec::aliases`, `spellings()`,
  alias-aware `split_name`, `/help` rendering, and tests (every spelling is
  reachable; `/exit` resolves to the very same row as `/quit`; `/help` lists
  every alias that dispatches)
- `crates/teton/tests/cli_e2e.rs` — the AC-5 equivalence test runs a third
  session that types `/exit`, and asserts byte-identical output to Ctrl-D plus
  no turn attempted
- `README.md` — the in-session command table names the alias

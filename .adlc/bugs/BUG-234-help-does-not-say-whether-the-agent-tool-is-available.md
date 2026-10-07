---
id: BUG-234
title: "/help does not say whether the agent tool is available, and a text-form call to a disabled agent gets the generic parser message"
status: open
severity: low
created: 2026-10-07
updated: 2026-10-07
component: "cli"
domain: "clients"
stack: ["rust", "cli", "daemon"]
concerns: ["developer-experience"]
tags: ["agent-tool", "help", "agent.enabled", "local-tier", "text-form-tool-call", "req-623"]
introduced_by: ["REQ-623"]
attribution: manual
---

## Description

REQ-623 BR-14 said the session's `/help` and the REQ-617 roster say whether
`agent` is available; the roster sentence shipped (conditional on the tool being
listed) but `/help` does not report it. Separately, on a local-tier route the
model emits text-form tool calls parsed by the daemon; a text-form call to
`agent` when `[agent] enabled = false` gets the parser's "not an available
tool" message rather than the hint naming `agent.enabled` that a native call
gets from the registry.

## Reproduction Steps

1. `[agent] enabled = false`; `/help` — no line about `agent`.
2. On a local route, have the model emit a text-form `agent` call — the
   refusal does not name `agent.enabled`.

## Expected Behavior

`/help` carries one line stating whether `agent` is registered this session;
both call forms get the `agent.enabled` hint.

## Actual Behavior

As described.

## Environment

- Platform: all
- Version: REQ-623 (919bccc)

## Root Cause

(filled during investigation)

## Resolution

(filled after fix)

## Files Changed

- (none yet)

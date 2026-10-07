---
id: BUG-233
title: "Five session-level events a child publishes carry no child_id, so a client attributes them to the parent"
status: open
severity: low
created: 2026-10-07
updated: 2026-10-07
component: "daemon/session"
domain: "harness"
stack: ["rust", "daemon", "json-rpc"]
concerns: ["developer-experience", "reliability"]
tags: ["agent-tool", "child-turns", "events", "child_id", "provider_degraded", "capability_dead_end", "req-623"]
introduced_by: ["REQ-623"]
attribution: manual
---

## Description

REQ-623 stamps `child_id`/`parent_turn_id` on `session_update`,
`context_pressure`, `cost_recorded` and `permission_request`, and suppresses
`route_decided`, `context_compacted`, `turn_queued`, `prefill_progress` and
`repo_context_state` inside a child (`harness::child::is_parent_only`). Five
kinds still leave a child unstamped: `provider_degraded`,
`capability_dead_end`, `prefix_cache`, `tool_call_repeated`,
`shell_duty_skipped`. The CLI renders the first two as Notice lines and the
activity row does not consume them, so today this is a labelling gap, not a
contract break — but a client cannot tell which child a degraded provider or a
repeated call belongs to.

## Reproduction Steps

1. Dispatch a child whose provider returns 529 so the child reroutes.
2. Observe `provider_degraded` on the session stream with no `child_id`.

## Expected Behavior

Every event a child publishes either carries the pair or is suppressed by
`is_parent_only`, and the list in `child.rs`'s doc is exhaustive.

## Actual Behavior

Five kinds are neither stamped nor suppressed.

## Environment

- Platform: all
- Version: REQ-623 (919bccc)

## Root Cause

The five payloads predate the optional pair and are published from sites that
do not go through `SessionEvents::emit`.

## Resolution

(filled after fix — additive optional fields per the REQ-588 vocabulary rule,
stamped by `for_child`; or add them to `is_parent_only` where a child has no
legitimate need to surface them)

## Files Changed

- (none yet)

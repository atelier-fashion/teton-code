---
id: BUG-232
title: "A timed-out or cancelled child's in-flight blocking tool is abandoned, not killed — its process can outlive the parent's turn"
status: open
severity: medium
created: 2026-10-07
updated: 2026-10-07
component: "daemon/harness"
domain: "harness"
stack: ["rust", "daemon"]
concerns: ["reliability", "security"]
tags: ["agent-tool", "child-turns", "shell", "cancellation", "deadline", "orphaned-process", "req-623"]
introduced_by: ["REQ-623"]
attribution: manual
---

## Description

When a child turn is cancelled or passes its deadline while a blocking tool
(`shell`, up to its own 120 s ceiling; `web`/`skill`/MCP through
`Handle::block_on`) is running, REQ-623 abandons the call: the status is
reported at once, the loop takes no further step (LESSON-664), its output never
reaches a model and its spend share is released only when the task ends. The
tool **process itself is not killed**: `statuses::timed_out` proves the
`.survived` marker appears after the deadline, and a shell can keep writing
files after the parent's turn has ended — so the next prompt can overlap with
it (REQ-567's linearity rule). This matches the parent's own cancelled-`shell`
behaviour today; REQ-623 AC-14/BR-10 were amended to say "abandoned, not
killed" and defer the kill here.

## Reproduction Steps

1. `[agent] child_deadline_secs = 2`; dispatch a child whose task runs
   `sleep 5 && touch survived`.
2. The child reports `timed_out` at ~2 s.
3. `survived` appears at ~5 s.

## Expected Behavior

The abandoned tool's process is terminated (or at least its writes are fenced)
when the child's status is reported, for children and for the parent alike.

## Actual Behavior

The process runs to completion; only its output is discarded.

## Environment

- Platform: all
- Version: REQ-623 (919bccc)

## Root Cause

`Tool::run` is synchronous and `shell` spawns without a cancellation handle;
the abort lands at the next await but cannot reach the child process.

## Resolution

(filled after fix)

## Files Changed

- (none yet)

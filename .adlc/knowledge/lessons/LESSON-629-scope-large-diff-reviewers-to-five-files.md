---
id: LESSON-629
title: "Scope large-diff reviewers to five files and cap their tool calls"
component: "adlc/proceed"
domain: "adlc"
stack: ["claude-code"]
concerns: ["review", "reliability"]
tags: ["phase-5", "agent-stall", "watchdog", "scoping"]
req: REQ-613
created: 2026-09-04
updated: 2026-09-04
---

## What Happened

REQ-613's Phase 5 diff was 61 files and about 14,000 lines. Five of the six review agents stalled at the 600-second stream watchdog: the reflector twice, correctness once, and quality, architecture and security each once. Reading the transcripts afterwards showed quality and architecture had reached "no findings" and were doing a last sweep; security had read the threat model and nothing else. Relaunching correctness and security scoped to the five core new files, with a tool-call cap and a word cap on the report, both finished in under four minutes and produced the findings that mattered (one critical, seven major).

## Lesson

For a diff over roughly 5,000 lines, do not hand a reviewer the whole diff. Pre-scope each dimension to at most five files, name the exact concerns to check, cap tool calls, and cap the report length. If an agent stalls twice on the same dimension, run that dimension's checklist in-context rather than relaunching a third time.

## Why It Matters

Every stall costs ten minutes of wall clock and reports nothing, and the three findings that would have shipped (a billed model call under a switched-off notes setting, a dead symlink guard, a scratch-name collision) were all found by the scoped relaunches, not the broad ones.

## Applies When

Dispatching review or audit agents in `/proceed` Phase 5, `/sprint`, or any fan-out over a diff larger than a few thousand lines.
